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
//! | 通道 | 能续别的 Mini App 拥有的会话吗 | 原因 |
//! | --- | --- | --- |
//! | `context.agent.run({ sessionId })` | ❌ | 规范原文：*Each Mini App owns its sessions and can continue only those* |
//! | 直连对方 MCP HTTP 端点 | ❌ | Host 的 MCP 客户端带 per-run 凭据，别的进程拿不到 |
//! | **起一个 Agent，让它调 `message_bot` MCP 工具** | ✅ | 2026-10-19 实测：唤起挂着 42212 的 bot，13 秒内回复 |
//!
//! ⇒ 守护进程负责**判定**（谁挂了、该不该救），把「请用 message_bot 唤醒这个 bot」
//! 写进请求文件；**cron 值守**（在主 Agent 目录里）负责**投递** —— 它有 `mavis`
//! 工具，能调 MCP 的 `message_bot`，而那会**复用目标 bot 自己的会话**。
//!
//! 与 Goal 唤醒共用同一条通道（同一个 cron 值守、同一个 acked 幂等表），
//! 只是请求的 `kind` 不同。

use std::path::PathBuf;

use serde_json::json;

/// Bot 守护请求文件（与 Goal 恢复请求同一个 cron 值守消费）。
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

/// MiniMax Bot 的插件数据目录（它的 state.json 在这里）。
fn bot_state_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| {
        PathBuf::from(h)
            .join(".minimax/v2/plugin-data/liveboards/minimax-bot/state.json")
    })
}

/// 请求的存活上限（毫秒）。与 Goal 恢复请求一致。
pub const RESUME_REQUEST_TTL_MS: i64 = 30 * 60 * 1000;

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

/// 轻量伪随机（xorshift）——只为抖动，不需要密码学强度。
fn pseudo_random() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x9E37_79B9_7F4A_7C15) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            x = 0x9E37_79B9_7F4A_7C15;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

/// 一个 bot 的会话信息。
#[derive(Debug, Clone)]
pub struct BotSession {
    pub bot_id: String,
    pub bot_name: String,
    pub session_id: String,
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
                });
            }
        }
    }
    out
}

/// 写一条「请唤醒这个 bot」的请求。
///
/// `text` 必须是给**cron 值守**看的指令（它会调 `message_bot`），
/// 不是给 bot 看的消息。
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
    requests.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    // 丢掉过期请求，别让 cron 为一个叫不醒的 bot 一直空转
    let now = crate::api::now_ms();
    requests.retain(|r| {
        let at = r.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
        now - at <= RESUME_REQUEST_TTL_MS
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
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default()))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// 已被 cron 值守确认处理的请求 id。
pub fn acked_ids() -> std::collections::BTreeSet<String> {
    let Some(path) = acked_path() else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("acked").and_then(|a| a.as_array()).cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 还没被处理的请求 id（供诊断与避免重复投递）。
pub fn pending_request_ids() -> std::collections::BTreeSet<String> {
    let Some(path) = request_path() else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Default::default() };
    let acked = acked_ids();
    let now = crate::api::now_ms();
    v.get("requests")
        .and_then(|r| r.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|x| {
                    let id = x.get("id").and_then(|i| i.as_str())?.to_string();
                    let at = x.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
                    if acked.contains(&id) || now - at > RESUME_REQUEST_TTL_MS {
                        return None;
                    }
                    Some(id)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 给 cron 值守的指令：让它用 `message_bot` 唤醒这个 bot。
pub fn revive_instruction(bot: &BotSession) -> String {
    format!(
        "用 minimax-bot 的 message_bot 工具，给 bot「{}」发送下面这条消息。\n\
         只发这一条，不要做别的事，也不要自己回答。\n\
         \n\
         消息正文：\n\
         【看门狗】上一个回合因内部错误中断，本条是自动重新启用的续跑消息。\n\
         请从中断处继续，不要重做已完成的部分。",
        bot.bot_name
    )
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

/// 一轮 Bot 守护。
///
/// 返回 (检查数, 可自愈数, 需人工数, 已投递数)。
pub fn tick(cfg: &crate::config::BotGuardCfg) -> Result<(usize, usize, usize, usize), String> {
    let bots = read_bot_sessions();
    if bots.is_empty() {
        return Ok((0, 0, 0, 0));
    }
    let states = read_session_states(&bots)?;
    let now = crate::api::now_ms();
    let mut self_healing = 0usize;
    let mut needs_user = 0usize;
    let mut revived = 0usize;
    let acked = acked_ids();

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
                crate::log(&format!(
                    "Bot 看门狗：{} 需要你处理（{}）—— 不自动重试",
                    bot.bot_name, message
                ));
                continue;
            }
            Verdict::SelfHealing => {
                // 已经在待办里 ⇒ 不重复投递
                let id = format!("bot-revive:{}:{}", bot.bot_id, bot.session_id);
                if acked.contains(&id) || pending_request_ids().contains(&id) {
                    continue;
                }
                self_healing += 1;
                match write_revive_request(bot, &revive_instruction(bot)) {
                    Ok(()) => {
                        revived += 1;
                        crate::log(&format!(
                            "Bot 看门狗：{} 因暂时性错误（{}）挂掉，已投递自动重新启用请求",
                            bot.bot_name, message
                        ));
                    }
                    Err(e) => crate::log(&format!("⚠️  Bot 看门狗：写 {} 的请求失败: {e}", bot.bot_name)),
                }
            }
        }
    }
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
