//! Goal 唤醒（把「已恢复状态」变成「真的重新开跑」）。
//!
//! # 为什么需要这个模块
//!
//! 2026-10-19 实测踩到的坑：**只把 `status` 改回 `active`，目标不会复活。**
//!
//! 运行时的目标循环（`@mavis/local-runtime/src/thread-goal/orchestrator.ts`）只在
//! **回合边界**读一次 durable goal 状态：
//!
//! ```text
//! while (true) {
//!   lastStatus = await runOneTurn(...)
//!   if (lastStatus === 'error') { pauseIfActiveAfterFailure(...); return }   // ← 跳出循环
//!   const goal = await store.getBySession(sessionId)
//!   if (!isContinuationStatus(goal.status)) return                            // ← paused 跳出
//!   promptText = await renderContinuation(goal); turnId = generateTurnId()
//! }
//! ```
//!
//! 所以目标一旦 `paused`，循环就**永久退出**；之后再把状态改回 `active`，
//! **没有任何东西会再读它一次** —— 目标就停在「状态是 active、但活没干」的死态。
//!
//! 运行时自己写下了这条契约（`continuation-rearm.ts` 头注释）：
//!
//! > **a writer that advances the decision epoch must take over the Goal's next
//! > step — by scheduling it, or by arming a marker here. A writer that takes
//! > over nothing must not advance it.**
//!
//! 而「the breaker reset is **a bare store write that publishes no lifecycle
//! event at all**」—— 裸写数据库**唤不醒目标**。本守护进程之前做的正是这件事。
//!
//! # 唤醒的唯一无头路径
//!
//! 运行时启动下一回合靠的是**往队列里投递一条续跑消息**
//! （`submitActiveContinuation` → `enqueuePostTurnContinuation`），
//! 落在 `local_runtime_queue_items` 表里。本模块照抄它的形状：
//!
//! - `source = 'api'`（与 `session send` 同一来源，已实测可送达）
//! - `client_request_id` = `mcode-reset-card-resume:<goalId>:<updatedAtMs>`
//!   —— 同一 epoch 内重试幂等，换 epoch 自然是新键
//! - `data_json` 内含 `origin.goalUpdatedAt`（epoch）与 `origin.objectiveDigest`
//!   （= sha256(objective)，已逐位验证）
//!
//! 队列是**运行时自己的传输层**，不是私造旁路：Host 的队列轮询器会认它。

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;

use crate::api::now_ms;

/// 队列项的 `routing_fingerprint`：与 Host 自己的写入逐字一致。
/// 换掉它，队列可能路由不到对应 agent。
const ROUTING_FINGERPRINT_GOAL: &str = "queue-routing/v1:[[\"source\",\"thread-goal\"],[\"platform\",null],[\"client\",null],[\"agent\",\"mavis\"],[\"agentBinding\",null],[\"binding\",null],[\"channel\",null],[\"channelId\",null],[\"thread\",null],[\"chat\",null],[\"scope\",null],[\"chatType\",null],[\"sender\",null]]";

/// 续跑项的 clientRequestId 前缀。**必须是这个前缀**：
/// 运行时的 `isThreadGoalContinuationQueueItem()` 用它来判断
/// 「这条队列项是不是目标续跑项」——
/// 前缀不对或没有 origin，就会被当成普通消息走通用分支，目标续跑通道根本不认。
const CONTINUATION_PREFIX: &str = "thread-goal-followup:";

/// 目标 + 会话的必要信息。
#[derive(Debug, Clone)]
pub struct WakeTarget {
    pub goal_id: String,
    pub session_id: String,
    pub objective: String,
    /// 目标的 epoch（`updated_at_ms`）。续跑消息必须带当前 epoch，否则会被当作过期项。
    pub goal_updated_at_ms: i64,
    pub agent_name: String,
    pub model: ModelSnapshot,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelSnapshot {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub context_limit: u64,
    #[serde(default)]
    pub thinking: Option<ThinkingSnapshot>,
    #[serde(default)]
    pub parameter_snapshot: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThinkingSnapshot {
    pub effort: String,
}

/// 从 `local_runtime_sessions.extra_data_json` 里解析模型快照。
///
/// 会话行里存着 `effectiveModel` / `effectiveModelVariant` /
/// `effectiveModelThinking.effort` / `effectiveModelContextWindow`，
/// 用它才能让唤醒后的回合**沿用用户当前选的模型**，而不是硬编码。
pub fn model_from_session_extra(extra_json: &str) -> Option<ModelSnapshot> {
    let v: serde_json::Value = serde_json::from_str(extra_json).ok()?;
    let raw_model = v.get("effectiveModel")?.as_str()?; // 形如 "minimax/MiniMax-M3.1-Flash-Preview"
    let (provider_id, model_id) = raw_model.split_once('/')?;
    let variant = v.get("effectiveModelVariant").and_then(|x| x.as_str());
    let reasoning = variant == Some("thinking");
    let thinking = v
        .get("effectiveModelThinking")
        .and_then(|x| x.get("effort"))
        .and_then(|x| x.as_str())
        .map(|effort| ThinkingSnapshot { effort: effort.to_string() });
    Some(ModelSnapshot {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        reasoning,
        context_limit: v
            .get("effectiveModelContextWindow")
            .and_then(|x| x.as_u64())
            .unwrap_or(1_000_000),
        thinking,
        parameter_snapshot: Some(json!({"context": "selection", "effort": "selection"})),
    })
}

/// 目标的 objective 摘要。运行时的 `digestThreadGoalObjective` 就是 sha256(objective)。
pub fn objective_digest(objective: &str) -> String {
    // 纯 sha256：不引依赖，自己算（32 字节摘要 = 64 个十六进制字符）
    let digest = sha256(objective.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 极简 SHA-256（只为算 objective 摘要；不引 crate 是为了保持这个守护进程的依赖面不变）。
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e;
            e = d.wrapping_add(t1);
            d = c; c = b; b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a); h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c); h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e); h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g); h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// 续跑消息正文。
///
/// 照运行时 `renderContinuationPrompt` 的口径：告诉模型「继续推进目标」，
/// 并明确「额度已恢复、从暂停处继续、不要重做已完成的部分」。
fn continuation_content(goal: &WakeTarget, used_percent: f64) -> String {
    format!(
        "【自动恢复】5 小时额度已回落到 {used_percent:.0}%，目标「{objective}」之前的暂停已解除。\n\
         请从暂停处继续执行当前目标：先确认当前状态与已完成的部分，不要重做；然后继续推进未完成的部分。",
        objective = goal.objective,
        used_percent = used_percent,
    )
}

/// 生成一个 RFC-4122 v4 形状的 uuid（够用即可，只需唯一）。
fn random_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // xorshift 混合，够随机到不撞键
    let mut x = nanos ^ 0x9E3779B97F4A7C15;
    let mut b = [0u8; 16];
    for i in 0..16 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        b[i] = (x >> (i % 8 * 8)) as u8;
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

/// 往队列表投递一条续跑消息。返回 `true` 表示已投递。
/// 写一条「恢复请求」，交给 Host 内的 MiniApp 去起回合。
///
/// 为什么要绕这一圈：守护进程跑在 Host **之外**，能改目标状态、能往队列表投
/// 续跑消息，但**唤不醒一个空闲的会话** —— 空闲会话没有调度器去轮询它的队列
/// （实测：一条 10 月 1 日投的续跑项在队列里躺了 18 天没被消费）。
/// 而「起回合」的能力（`context.agent.run`）只有 Host **内部**才有。
///
/// 于是用本地文件做交接：两边看同一个文件，不需要网络、不需要凭据。
/// 请求 id 带 epoch（`resume:<goalId>:<epoch>`），同一 epoch 重复写也只会被
/// MiniApp 处理一次。
pub fn write_resume_request(goal: &WakeTarget, used_percent: f64) -> Result<(), String> {
    let path = request_path().ok_or("找不到 $HOME")?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 读现有内容（损坏就当空）
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut requests: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&existing)
        .ok()
        .and_then(|v| v.get("requests").and_then(|r| r.as_array()).cloned())
        .unwrap_or_default();

    let id = format!("resume:{}:{}", goal.goal_id, goal.goal_updated_at_ms);
    requests.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    // 顺手丢掉过期的请求，别让投递端为一个叫不醒的目标一直空转
    let now = now_ms();
    requests.retain(|r| {
        let at = r.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
        now - at <= RESUME_REQUEST_TTL_MS
    });
    requests.push(json!({
        "id": id,
        "goalId": goal.goal_id,
        "sessionId": goal.session_id,
        "epoch": goal.goal_updated_at_ms,
        "text": continuation_content(goal, used_percent),
        "atMs": now,
    }));
    // 只留最近 50 条
    if requests.len() > 50 {
        requests = requests.split_off(requests.len() - 50);
    }
    let body = json!({ "requests": requests });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default()))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;

    // ⚠️ 同一 epoch 的请求可能之前已被 ack 过。若不把它从 ack 列表里摘掉，
    // 这条请求就会被当成「已处理」而永远不再被投递（实测踩过）。
    unack(&id);
    Ok(())
}

/// 把某个 id 从「已处理」列表里摘掉，让它重新变成待处理。
fn unack(id: &str) {
    let path = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/goal-resume-acked.json"));
    let Some(path) = path else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let mut acked: Vec<String> = v
        .get("acked")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let before = acked.len();
    acked.retain(|x| x != id);
    if acked.len() == before {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(
        &tmp,
        format!("{}\n", serde_json::to_string_pretty(&json!({ "acked": acked })).unwrap_or_default()),
    )
    .is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn request_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/goal-resume-requests.json"))
}

/// 已处理（acked）的请求 id 集合。
pub fn acked_ids() -> std::collections::BTreeSet<String> {
    let path = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/goal-resume-acked.json"));
    let Some(path) = path else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Default::default() };
    v.get("acked")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 恢复请求的时效上限：超过这么久还没被处理就作废。
///
/// 目的有两个：
/// 1. 避免投递端为一个叫不醒的目标无限期空转；
/// 2. 避免「会话根本叫不醒」时，请求永远挂着、状态永远显示「待处理」。
pub const RESUME_REQUEST_TTL_MS: i64 = 30 * 60 * 1000;

/// 请求文件里还没被处理的 id（且未过期）。
pub fn pending_request_ids() -> std::collections::BTreeSet<String> {
    let path = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/goal-resume-requests.json"));
    let Some(path) = path else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Default::default() };
    let acked = acked_ids();
    let now = now_ms();
    v.get("requests")
        .and_then(|r| r.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|x| {
                    let id = x.get("id").and_then(|i| i.as_str())?.to_string();
                    let at = x.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
                    if acked.contains(&id) { return None; }
                    if now - at > RESUME_REQUEST_TTL_MS { return None; }
                    Some(id)
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn enqueue_continuation(
    db: &PathBuf,
    goal: &WakeTarget,
    used_percent: f64,
) -> Result<bool, String> {
    let item_id = format!("queue_{}", random_uuid());
    let user_message_id = format!("msg-user-v1-{}", random_uuid().replace('-', ""));
    let turn_id = format!("turn_{}", random_uuid());
    // 必须是 `thread-goal-followup:` 前缀 —— 运行时靠它识别「这是目标续跑项」。
    // 用的是运行时自己的 rearm 键形状：同一 epoch 内重试幂等，换 epoch 自然是新键。
    let client_request_id = format!(
        "{CONTINUATION_PREFIX}rearm:{}:{}",
        goal.goal_id, goal.goal_updated_at_ms
    );

    let message = json!({
        "content": continuation_content(goal, used_percent),
        "attachments": [],
        "hideUserMessage": true,
        "displayAttachments": [],
        "origin": {
            "type": "thread-goal-continuation",
            "goalId": goal.goal_id,
            "goalUpdatedAt": goal.goal_updated_at_ms,
            "objectiveDigest": objective_digest(&goal.objective),
            "kind": "active",
        },
    });
    let model = json!({
        "provider_id": goal.model.provider_id,
        "model_id": goal.model.model_id,
        "reasoning": goal.model.reasoning,
        "context_limit": goal.model.context_limit,
        "thinking": goal.model.thinking.as_ref().map(|t| json!({"effort": t.effort})),
        "parameterSnapshot": goal.model.parameter_snapshot.clone().unwrap_or(json!({"context":"selection","effort":"selection"})),
    });
    let data = json!({
        "itemId": item_id,
        "userMessageId": user_message_id,
        "sessionId": goal.session_id,
        "agentName": goal.agent_name,
        "source": "thread-goal",
        "status": "queued",
        "message": message,
        "model": model,
        "createdAt": now_ms(),
        "requestedTurnId": turn_id,
        "clientRequestId": client_request_id,
    });

    // ⚠️ 必须**先查后插**，不能只靠 `INSERT OR IGNORE`：
    // 真实运行库里 `idx_local_runtime_queue_items_session_client_request` **不是唯一索引**
    // （迁移源文件里写的是 UNIQUE，但库里实际是普通 CREATE INDEX）。
    // 实测：同 crid 反复插会插出 6 条重复项，把目标会话的队列堵死，
    // 结果 `session send` 只能排队、起不了回合。
    let dup = format!(
        "SELECT COUNT(*) FROM local_runtime_queue_items\n\
         WHERE session_id = '{sid}' AND client_request_id = '{crid}';",
        sid = goal.session_id.replace('\'', "''"),
        crid = client_request_id,
    );
    if let Ok(out) = crate::goal_sync::exec_sql(db, &dup) {
        let n: i64 = out
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim()
            .parse()
            .unwrap_or(0);
        if n > 0 {
            // 已经有同 epoch 的续跑项在队列里，别再塞一份
            return Ok(false);
        }
    }

    // 同一目标的**旧 epoch** 残留项也要清掉：它们永远不会被消费（epoch 对不上会被
    // 准入判据 cancel），却会一直占着队列位置。
    let stale = format!(
        "DELETE FROM local_runtime_queue_items\n\
         WHERE session_id = '{sid}' AND source = 'thread-goal'\n\
         AND client_request_id LIKE 'thread-goal-followup:rearm:{gid}:%'\n\
         AND client_request_id <> '{crid}';",
        sid = goal.session_id.replace('\'', "''"),
        gid = goal.goal_id.replace('\'', "''"),
        crid = client_request_id,
    );
    let _ = crate::goal_sync::exec_sql(db, &stale);

    // client_request_id 上有唯一索引，重复投递会被数据库挡住 ⇒ 天然幂等。
    let sql = format!(
        "INSERT OR IGNORE INTO local_runtime_queue_items\n\
         (session_id, item_id, status, created_at_ms, data_json, source, client_request_id, routing_fingerprint)\n\
         VALUES ('{sid}', '{item}', 'queued', {now}, '{data}', 'thread-goal', '{crid}', '{fp}');",
        sid = goal.session_id.replace('\'', "''"),
        item = item_id,
        now = now_ms(),
        data = data.to_string().replace('\'', "''"),
        crid = client_request_id,
        fp = ROUTING_FINGERPRINT_GOAL,
    );
    crate::goal_sync::exec_sql(db, &sql)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // 空串的 sha256 是公认常量
        assert_eq!(
            objective_digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            objective_digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn objective_digest_matches_runtime() {
        // 2026-10-19 实测：tg_im1jszv4sjmuoen4o4 的 objective 摘要
        assert_eq!(
            objective_digest("你帮我继续完成剩余的内容"),
            "e0a1d0f4cd6d33780a781ef5802c6344f0cbfa4afdf6619fb76b2eebd4aa4eb4"
        );
    }

    #[test]
    fn model_snapshot_comes_from_session_extra() {
        let extra = r#"{"effectiveModel":"minimax/MiniMax-M3.1-Flash-Preview",
            "effectiveModelVariant":"thinking",
            "effectiveModelThinking":{"effort":"max"},
            "effectiveModelContextWindow":1000000}"#;
        let m = model_from_session_extra(extra).expect("应能解析");
        assert_eq!(m.provider_id, "minimax");
        assert_eq!(m.model_id, "MiniMax-M3.1-Flash-Preview");
        assert!(m.reasoning);
        assert_eq!(m.context_limit, 1_000_000);
        assert_eq!(m.thinking.unwrap().effort, "max");
    }

    #[test]
    fn uuid_is_unique_looking() {
        let a = random_uuid();
        let b = random_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }
}
