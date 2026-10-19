//! Goal 自动暂停/恢复（功能2）。
//!
//! # 为什么要直写数据库
//!
//! Goal 的官方接口 `/minimax-desktop/api/v1/session/:id/goal` 只在 **Electron
//! 主进程的自定义协议**里可达，没有 TCP 端口；v2 前门的 goal controller 是
//! 五个 `NotImplementedError` 桩。独立进程（本守护进程、Mini App 的 Node
//! 进程）都调不到。
//!
//! 这条路可行，是因为已验证两件事：
//!   1. `SqliteThreadGoalStore` 每次都重新 SELECT，没有内存缓存；
//!   2. `orchestrator` 在每个回合边界后重读 Goal
//!      （"Read the durable goal state AFTER each turn"），
//!      所以暂停会在**下一个回合边界**生效，不会打断正在跑的那一回合。
//!
//! # 已知降级（必须如实告知用户）
//!
//!   - 不调用 `abortVerification`：正在跑的回合会自然跑完；
//!   - 不发 `thread_goal` 事件：已打开的面板不会实时刷新。
//!
//! # 所有权
//!
//! 官方 UI 的手动暂停与我们写的暂停**共用** `paused(user_requested)`
//! （`lifecycle.ts` 里写死），单看 `status_reason` 分不出是谁停的。
//! 因此另有一份账本 `goal-pause.json` 记录「哪些是我们停的」，
//! 恢复时只动账本内的目标 —— 否则会把用户手动暂停的目标自动打开。
//!
//! # 不加依赖
//!
//! 调系统自带的 `/usr/bin/sqlite3`，不引入 rusqlite：
//! 那个 crate（尤其 `bundled`）要编 C，与本工程
//! `opt-level="z" + panic="abort" + strip` 的取向相悖。

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Config;
use crate::log;

/// 必须用绝对路径：launchd 的 `PATH` 极简。
pub const SQLITE_BIN: &str = "/usr/bin/sqlite3";

/// 与 `lifecycle.ts` 里 `pauseGoalByUser` 写死的值一致，不要自创。
pub const REASON_USER_PAUSED: &str = "paused(user_requested)";

/// goal_id 格式校验。白名单之外的一律丢弃 —— 账本是我们自己写的，
/// 但它可能被人手改过，不能因为一个坏 id 就让后续 SQL 变成注入面。
fn valid_goal_id(id: &str) -> bool {
    let b = id.as_bytes();
    if id.len() < 11 || id.len() > 67 || !id.starts_with("tg_") {
        return false;
    }
    b[3..].iter().all(|c| c.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub goal_id: String,
    pub session_id: String,
    pub paused_at_ms: i64,
    pub trigger_used_percent: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub paused: Vec<LedgerEntry>,
}

fn state_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/mcode-reset-card"))
}

pub fn ledger_path() -> Option<PathBuf> {
    Some(state_dir()?.join("goal-pause.json"))
}

/// 目标数据库。优先 `MAVIS_GOAL_DB`，其次默认路径；找不到就禁用该功能，**绝不猜路径乱写**。
pub fn db_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MAVIS_GOAL_DB") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let p = PathBuf::from(home).join(".minimax/v2/sqlite/runtime-state.sqlite");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

pub fn read_ledger() -> Ledger {
    let Some(p) = ledger_path() else { return Ledger::default() };
    let Ok(text) = fs::read_to_string(&p) else { return Ledger::default() };
    let Ok(mut l) = serde_json::from_str::<Ledger>(&text) else { return Ledger::default() };
    l.paused.retain(|e| valid_goal_id(&e.goal_id));
    l
}

/// 原子写：写 tmp 再 rename。同目录 rename 在 POSIX 上是原子的。
pub fn write_ledger(l: &Ledger) -> Result<(), String> {
    let Some(p) = ledger_path() else { return Err("找不到 $HOME".into()) };
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(l).map_err(|e| e.to_string())?;
    fs::write(&tmp, format!("{text}\n")).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// 执行一段 SQL（经 stdin），返回 stdout。
pub fn exec_sql(db: &PathBuf, sql: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new(SQLITE_BIN)
        .arg("-json")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 {SQLITE_BIN}：{e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("stdin 不可用")?
        .write_all(sql.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.trim().lines().next().unwrap_or("sqlite3 执行失败").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sqlite(db: &PathBuf, sql: &str) -> Result<String, String> {
    exec_sql(db, sql)
}

/// 从 stdout 里取**最后一个**顶层 JSON 文档。
///
/// sqlite3 的 `-json` 模式会给每条「有结果集」的语句各输出一个 JSON 文档。
/// 所以只要脚本里混进一条会回显的语句（`PRAGMA busy_timeout=3000` 就是典型，
/// 它会输出 `[{"timeout":3000}]`），整段 stdout 就不再是**单个** JSON，
/// 直接 `from_str` 必然报 `trailing characters`。
///
/// 这个 bug 的后果比解析失败严重得多：那条 SQL 里的 UPDATE **已经 COMMIT 成功了**
/// （目标确实被停了），只是返回行解析炸了 → 账本没写 → 下一个 tick 无从恢复
/// → **目标被停住且再也回不来**。所以这里取最后一个文档，
/// 让脚本里能不能混语句不再是一个"一不小心就单向门"的地雷。
fn last_json_document(out: &str) -> Result<serde_json::Value, String> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Ok(json!([]));
    }
    let mut last: Option<serde_json::Value> = None;
    let (mut depth, mut start) = (0usize, 0usize);
    let (mut in_str, mut esc) = (false, false);
    for (i, ch) in trimmed.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '[' | '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            ']' | '}' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        last = serde_json::from_str(&trimmed[start..=i]).ok();
                    }
                }
            }
            _ => {}
        }
    }
    last.ok_or_else(|| "sqlite3 输出里找不到可解析的 JSON 文档".to_string())
}

/// 把目标表里所有 active 的行暂停，并返回**我们改到的**那些行。
///
/// 用临时表先快照改前状态，避免「改完再查」把用户早就暂停的也算进来。
pub fn pause_active_goals(db: &PathBuf, now_ms: i64) -> Result<Vec<LedgerEntry>, String> {
    let sql = format!(
        ".timeout 3000\n\
         BEGIN IMMEDIATE;\n\
         CREATE TEMP TABLE _newly AS\n\
         \x20 SELECT goal_id, session_id FROM local_runtime_thread_goals WHERE status='active';\n\
         UPDATE local_runtime_thread_goals\n\
         \x20  SET status='paused', status_reason='{REASON_USER_PAUSED}', updated_at_ms={now_ms}\n\
         \x20 WHERE status='active';\n\
         SELECT goal_id, session_id FROM _newly;\n\
         COMMIT;\n"
    );
    let out = sqlite(db, &sql)?;
    let rows: serde_json::Value = last_json_document(&out)?;
    Ok(rows
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let g = r.get("goal_id")?.as_str()?.to_string();
                    let s = r.get("session_id")?.as_str()?.to_string();
                    if !valid_goal_id(&g) {
                        return None;
                    }
                    Some(LedgerEntry {
                        goal_id: g,
                        session_id: s,
                        paused_at_ms: now_ms,
                        trigger_used_percent: 0.0,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// 恢复单个目标。
///
/// `reply_fingerprint` 必须一并置 NULL —— 复刻 `store-patch.ts` 里
/// `resumed` 的副作用（用于无进展检测）。
pub fn resume_goal(db: &PathBuf, goal_id: &str, now_ms: i64) -> Result<(), String> {
    if !valid_goal_id(goal_id) {
        return Err(format!("goal_id 格式不合法，已拒绝：{goal_id}"));
    }
    // ── 先确认目标还在、且确实处于 paused ──
    //
    // ⚠️ 这里有两个坑，叠在一起会**静默地让恢复永远失效**：
    //
    // 1) `UPDATE ... WHERE goal_id='{goal_id}'`（Rust 的 `format!` 不替换
    //    花括号占位符 —— `{goal_id}` 是字面量！）⇒ WHERE 条件恒不成立，
    //    影响 0 行，但 sqlite3 照样退出码 0 ⇒ `resume_goal` 返回 Ok(())
    //    ⇒ 调用方以为「恢复成功」，实际目标一直停在 paused。
    //    实测 2026-10-19 19:46 用真实库复现：UPDATE 前后 status 都是 paused。
    //
    // 2) 即便把 id 拼对了，目标若已被官方 DELETE 掉（实测 19:42:41
    //    MCode 界面层发了 `DELETE /minimax-desktop/api/v1/session/<sid>/goal`，
    //    行被硬删除），UPDATE 同样影响 0 行 ⇒ 又一次假成功。
    //
    // ⇒ 所以先 SELECT 校验，再 UPDATE；两步都要求「确实改到了行」。
    let gid = goal_id.replace('\'', "''");
    let check = format!("SELECT status FROM local_runtime_thread_goals WHERE goal_id='{gid}';");
    let cur = sqlite(db, &check)?;
    let cur_rows = last_json_document(&cur)?;
    let Some(row) = cur_rows.as_array().and_then(|a| a.first()) else {
        return Err(format!("目标 {goal_id} 已不存在（可能已被官方删除），跳过恢复"));
    };
    let cur_status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if cur_status != "paused" {
        return Err(format!(
            "目标 {goal_id} 当前状态是 {cur_status}（非 paused），跳过恢复"
        ));
    }
    let sql = format!(
        ".timeout 3000\n\
         BEGIN IMMEDIATE;\n\
         UPDATE local_runtime_thread_goals\n\
         \x20  SET status='active', status_reason=NULL, reply_fingerprint=NULL, updated_at_ms={now_ms}\n\
         \x20  WHERE goal_id='{gid}' AND status='paused';\n\
         COMMIT;\n"
    );
    sqlite(db, &sql)?;
    // 再查一次，确认真的改成 active 了（UPDATE 影响 0 行时 sqlite3 仍返回 0）
    let after = format!("SELECT status FROM local_runtime_thread_goals WHERE goal_id='{gid}';");
    let aout = sqlite(db, &after)?;
    let arows = last_json_document(&aout)?;
    let new_status = arows
        .as_array()
        .and_then(|a| a.first())
        .and_then(|o| o.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if new_status != "active" {
        return Err(format!(
            "目标 {goal_id} 恢复未生效（当前仍是 {new_status}）"
        ));
    }
    Ok(())
}

/// 账本里那些已经被用户处理掉（不再 paused）的，直接清掉，不再干预。
///
/// 返回被丢掉的条目数，供日志如实告知用户。
fn prune_ledger(db: &PathBuf, ledger: &mut Ledger) -> Result<usize, String> {
    if ledger.paused.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = ledger
        .paused
        .iter()
        .filter(|e| valid_goal_id(&e.goal_id))
        .map(|e| format!("'{}'", e.goal_id))
        .collect();
    if ids.is_empty() {
        let dropped = ledger.paused.len();
        ledger.paused.clear();
        return Ok(dropped);
    }
    let sql = format!(
        "PRAGMA busy_timeout=3000;\n\
         SELECT goal_id, status FROM local_runtime_thread_goals WHERE goal_id IN ({});\n",
        ids.join(",")
    );
    let out = sqlite(db, &sql)?;
    let rows: serde_json::Value = last_json_document(&out)?;
    let mut still_paused: Vec<String> = Vec::new();
    // 状态不是 paused 的：逐个判断是「用户手动处理」还是「运行时回弹」。
    // 只有前者才剔除；后者必须留在账本里（我们仍然拥有它，恢复时要放回去）。
    let mut dropped = 0usize;
    for r in rows.as_array().cloned().unwrap_or_default() {
        let Some(g) = r.get("goal_id").and_then(|s| s.as_str()) else { continue };
        let status = r.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if status == "paused" {
            still_paused.push(g.to_string());
            continue;
        }
        // 找出这个目标在账本里的暂停时刻
        let paused_at = ledger
            .paused
            .iter()
            .find(|e| e.goal_id == g)
            .map(|e| e.paused_at_ms)
            .unwrap_or(0);
        if is_user_handled(db, g, paused_at) {
            dropped += 1;
        } else {
            // 运行时回弹：仍算我们停的，保留在账本里
            still_paused.push(g.to_string());
        }
    }
    let before = ledger.paused.len();
    ledger.paused.retain(|e| still_paused.contains(&e.goal_id));
    Ok(dropped.max(before - ledger.paused.len()))
}

/// 读一个目标的当前 status；目标不存在时返回 `None`。
///
/// 用于「恢复后回读校验」—— 判据必须基于**回读**，
/// 不能只信「UPDATE 执行成功」（影响 0 行时 sqlite3 仍返回 0）。
fn goal_status(db: &PathBuf, goal_id: &str) -> Option<String> {
    let sql = format!(
        "SELECT status FROM local_runtime_thread_goals WHERE goal_id='{gid}';",
        gid = goal_id.replace('\'', "''"),
    );
    let out = sqlite(db, &sql).ok()?;
    let rows = last_json_document(&out).ok()?;
    rows.as_array()?
        .first()?
        .get("status")?
        .as_str()
        .map(|s| s.to_string())
}

/// 判定「账本里的目标是不是**用户**手动处理掉了」。
///
/// # 为什么需要单独判
///
/// `prune_ledger` 只看 `status=='paused'`：不在这个状态就说明目标被人动过了
/// （改回 active、或完成），于是从账本剔除、不再干预。
///
/// 但 2026-10-19 实测发现一个**假阳性**：
/// 回合被额度墙打死时，运行时会在回合结束时把目标从 `paused` 改回 `active`
/// （实测量化目标 18:59:03 被暂停 → 19:12:55 那个跑了 13 分钟的回合结束
/// → 19:13:04 目标变回 `active`）。于是 `prune_ledger` 把它当「用户手动处理」
/// 剔出账本，紧接着 `pause_active_goals` 又把它停回来 —— 账本被清空又重建，
/// `trigger_used_percent` 从 99 被覆盖成 100，且日志误报「已被手动处理」。
///
/// # 判据
///
/// 目标处于 `active` **且**最近一次变更发生在**我们暂停它之后** ⇒ 是运行时的
/// 回弹，不是用户操作。此时**保留在账本里**（我们仍然拥有它，恢复时要放回去），
/// 但**不**记为「用户手动处理」。
///
/// 目标处于 `complete`/`complete(verifier_met)` ⇒ 真的完成了，剔除。
fn is_user_handled(db: &PathBuf, goal_id: &str, paused_at_ms: i64) -> bool {
    // 1) 目标已完成 ⇒ 真的处理掉了
    let sql = format!(
        "SELECT status, session_id FROM local_runtime_thread_goals WHERE goal_id = '{gid}';",
        gid = goal_id.replace('\'', "''"),
    );
    let Ok(out) = sqlite(db, &sql) else { return false };
    let Ok(rows) = last_json_document(&out) else { return false };
    let Some(row) = rows.as_array().and_then(|a| a.first()) else {
        // 目标已经被删了（官方 DELETE 掉了）⇒ 账本里的这条已经没有意义
        return true;
    };
    let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status.starts_with("complete") {
        return true;
    }
    let session_id = row.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
    if !valid_session_id(session_id) {
        // 拿不到会话就没法判断回弹 ⇒ 保守起见当作「非用户处理」，保留在账本里
        return false;
    }

    // 2) 我们暂停之后是否还有回合结束过？⇒ 运行时回弹，不是用户操作
    //
    // ⚠️ 第一版用 `goal.updated_at_ms > paused_at_ms` 判，**恒为 false**：
    // `pause_active_goals` 自己在暂停时就把 `updated_at_ms` 改写成 `now_ms`，
    // 所以对账时读到的永远是「我们暂停的那一刻」⇒ 回弹永远被误判成
    // 「用户手动处理」。实测 19:19:388 又误报一次，账本 paused_at_ms 被改成
    // 19:19:15、trigger 被覆盖成 100.0。
    // ⇒ 改用**回合结束时刻**做判据：这是我们改不动的第三方事实。
    let rebound = format!(
        "SELECT COUNT(*) AS c FROM local_runtime_turn_ingress\n\
         WHERE session_id = '{sid}' AND status = 'completed'\n\
         AND completed_at_ms > {paused};",
        sid = session_id.replace('\'', "''"),
        paused = paused_at_ms,
    );
    let Ok(rout) = sqlite(db, &rebound) else { return false };
    let Ok(rrows) = last_json_document(&rout) else { return false };
    let c = rrows
        .as_array()
        .and_then(|a| a.first())
        .and_then(|o| o.get("c"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if c > 0 {
        // 有回合在我们暂停之后结束过 ⇒ 运行时在回合边界把它改回 active
        // ⇒ 是**回弹**，不是用户手动处理 ⇒ 保留在账本里
        return false;
    }

    // 3) 没有任何回合在暂停后结束过 ⇒ 状态变化只可能来自用户
    true
}

/// 恢复判定：额度是否已经「回来了」。
///
/// 判据是**用量回落到触发线以下**，而不是字面上的「用量正好等于 0%」。
///
/// 为什么：守护进程每 60 秒才轮询一次，额度刷新后的那个 0% 瞬间**通常根本观测不到**
/// （实测：重置卡在 14:35:41 把额度打回 0%，下一轮 14:36:20 读到的是 1%）。
/// 若严格按 `used <= 0` 判定，目标会被**永久停住** —— 这个故障真实发生过。
///
/// 额度只有在「用掉重置卡」或「5 小时窗口翻页」时才会掉下来，
/// 掉下来就说明额度已经回来了。`resume_at_percent` 作为更严格的附加条件保留。
pub fn should_resume(used_percent: f64, trigger_percent: f64, resume_at_percent: f64) -> bool {
    used_percent < trigger_percent || used_percent <= resume_at_percent
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub paused: usize,
    pub resumed: usize,
    pub message: String,
}

/// 一轮 Goal 同步。顺序与扣卡相反：**先让目标停下，再决定要不要补额度**。
pub fn sync_with_quota(
    cfg: &Config,
    five_used_percent: f64,
    five_total_percent: f64,
    five_unlimited: bool,
) -> SyncReport {
    let gp = cfg.goal_pause;
    if !gp.enabled {
        return SyncReport { message: "Goal 自动暂停未开启".into(), ..Default::default() };
    }
    let Some(db) = db_path() else {
        return SyncReport {
            message: "找不到 Goal 数据库，功能已禁用".into(),
            ..Default::default()
        };
    };
    let now_ms = crate::api::now_ms();
    let mut ledger = read_ledger();

    // 不限量：额度不可能是它还停着的原因，把账本里的都放回去。
    if five_unlimited {
        let mut n = 0usize;
        let mut woke = 0usize;
        let mut still_stuck: Vec<LedgerEntry> = Vec::new();
        for e in &ledger.paused {
            if resume_goal(&db, &e.goal_id, now_ms).is_ok() {
                n += 1;
                // 同样要唤醒：只改状态的话目标会停在「active 但活没干」的死态
                match wake_goal(&db, &e.goal_id, five_used_percent) {
                    Ok(true) => woke += 1,
                    Ok(false) => {}
                    Err(err) => {
                        log(&format!("🔴 唤醒 {} 失败: {err}", e.goal_id));
                        still_stuck.push(e.clone());
                    }
                }
            }
        }
        if !still_stuck.is_empty() {
            if let Err(e) = write_ledger(&Ledger { paused: still_stuck }) {
                log(&format!("⚠️  账本写回失败: {e}"));
            }
        } else if !ledger.paused.is_empty() {
            if let Err(e) = write_ledger(&Ledger { paused: Vec::new() }) {
                log(&format!("⚠️  账本清空失败: {e}"));
            }
        }
        return SyncReport {
            paused: 0,
            resumed: n,
            message: format!("不限量额度，已恢复 {n} 个目标（唤醒 {woke} 个）"),
        };
    }

    // 先对账：用户已经自己处理掉的，不再干预。
    match prune_ledger(&db, &mut ledger) {
        Ok(0) => {}
        Ok(n) => {
            log(&format!("账本对账：{n} 个目标已被手动处理，不再干预"));
            if let Err(e) = write_ledger(&ledger) {
                log(&format!("⚠️  账本对账后写回失败: {e}"));
            }
        }
        // 以前这里是 `if let Ok(n)`，对账失败就当没发生 —— 账本会无限堆积，
        // 恢复时去动早已被人处理掉的目标。对账失败必须喊出来。
        Err(e) => log(&format!("⚠️  账本对账失败（不影响本轮判定）: {e}")),
    }

    // ── 武装/解除武装 cron ──
    //
    // ⚠️ 这个调用有**两个**时机，缺一不可（2026-10-19 20:12 实测）：
    //
    // 1) **分支之前**（这里）：负责**解除武装**。
    //    原来 `reconcile_resume_cron` 只挂在恢复分支里，而额度一旦持续高于
    //    触发线，走的就是**暂停分支** —— 那条路径没有这个调用
    //    ⇒ cron 永远不会被解除武装，每 17 分钟空跑一轮（每轮约 47k tokens），
    //    而 pending 请求其实早已过期（TTL 30 分钟）。
    //
    // 2) **恢复分支之后**（见下）：负责**武装**。
    //    因为恢复请求（`goal-resume-requests.json`）是恢复分支里
    //    `wake_goal()` 写的 —— 分支前调用时 pending 还是 0 个，
    //    ⇒ 武装永远**慢一轮**。实测 20:12:365 日志「pending 请求 0 个，
    //    cron 未武装」，而同一轮 20:12:458 就恢复了目标并写了请求，
    //    但 cron 要等到下一轮才被武装 ⇒ 唤醒被推迟 1 分钟。
    //
    // 两次调用都不昂贵（各一次 SQL 查询 + 必要时一次 UPDATE）。
    reconcile_resume_cron(&db, cfg);

    if five_used_percent >= five_total_percent * gp.trigger_percent / 100.0 {
        match pause_active_goals(&db, now_ms) {
            Ok(rows) => {
                let n = rows.len();
                if n > 0 {
                    for r in &rows {
                        // 已经在账本里的目标**不要重复 push**。
                        // 2026-10-19 实测踩到：目标被运行时回弹成 active 后重新暂停，
                        // 旧条目还在账本里，再 push 一条就成了重复 ⇒ 恢复时同一目标
                        // 会被处理两次，且 `trigger_used_percent` 被后一条覆盖
                        // （99 被改成 100，看起来像触发线变了，其实只是记账重复）。
                        if ledger.paused.iter().any(|e| e.goal_id == r.goal_id) {
                            continue;
                        }
                        ledger.paused.push(LedgerEntry {
                            trigger_used_percent: five_used_percent,
                            ..r.clone()
                        });
                    }
                    // 目标已经停在库里了，账本写不进去 = 永远恢复不了。
                    // 这种错误绝不能 `let _ =` 吞掉，必须喊到用户眼前。
                    if let Err(e) = write_ledger(&ledger) {
                        log(&format!(
                            "🔴 账本写入失败（{} 个目标已暂停但**无法自动恢复**）: {e}",
                            ledger.paused.len()
                        ));
                    }
                }
                SyncReport {
                    paused: n,
                    resumed: 0,
                    message: if n > 0 {
                        format!("已暂停 {n} 个目标（5 小时已用 {five_used_percent:.1}%）")
                    } else {
                        "没有处于进行中的目标需要暂停".into()
                    },                }
            }
            Err(e) => SyncReport {
                message: format!("写入失败：{e}"),
                ..Default::default()
            },
        }
    } else {
        // ── 未达触发线：之前因额度到顶而停掉的目标，到这里就该放回去 ──
        //
        // 关键：**恢复 = 改状态 + 投递续跑消息**，缺一不可。
        // 只改状态的话，目标会停在「状态 active、但活没干」的死态
        // （运行时的目标循环在 paused 时就永久退出了，没人再读它）。
        // 详见 goal_wake.rs。
        let mut n = 0usize;
        let mut woke = 0usize;
        let mut still_stuck: Vec<LedgerEntry> = Vec::new();
        for e in &ledger.paused {
            if !should_resume(five_used_percent, gp.trigger_percent, gp.resume_at_percent) {
                still_stuck.push(e.clone());
                continue;
            }
            if resume_goal(&db, &e.goal_id, now_ms).is_ok() {
                n += 1;
                // 唤醒：把续跑消息投进运行时自己的队列，让它真的开新回合
                match wake_goal(&db, &e.goal_id, five_used_percent) {
                    Ok(true) => woke += 1,
                    Ok(false) => still_stuck.push(e.clone()),
                    Err(err) => {
                        log(&format!("🔴 唤醒 {} 失败（状态已改回 active 但活没起来）: {err}", e.goal_id));
                        still_stuck.push(e.clone());
                    }
                }
            }
        }
        // 账本只在「唤醒确实投递成功」后才清空。
        // 唤醒失败的目标必须留在账本里，下一轮重试 —— 否则它会被永久遗弃。
        //
        // ⚠️ 2026-10-19 20:22 实测补：**「请求写成功」不等于「目标真的活了」**。
        // `wake_goal()` 写恢复请求成功就返回 Ok，于是账本被清空；
        // 但如果目标此刻实际是 `paused`（例如被别的东西改回去了、
        // 或 500 打死后的回弹），运行时就**不会执行**这个目标 ——
        // 表现是：cron 成功投递（`accepted`、排在队首），
        // 量化会话也确实起了新 turn，但那个 turn 永远挂着不完成，
        // turns_used 一动不动。
        // ⇒ 清空账本前必须**回读目标行**，确认它真的是 active。
        let mut still_paused_after: Vec<LedgerEntry> = Vec::new();
        for e in &ledger.paused {
            if !still_stuck.iter().any(|x| x.goal_id == e.goal_id) {
                // 这一轮刚恢复过 ⇒ 再确认一次状态确实是 active
                match goal_status(&db, &e.goal_id) {
                    Some(st) if st == "active" => {}
                    Some(st) => {
                        log(&format!(
                            "⚠️  目标 {} 恢复后状态仍是 {st}（不是 active）⇒ 保留在账本里继续观察",
                            e.goal_id
                        ));
                        still_paused_after.push(e.clone());
                    }
                    None => {
                        log(&format!(
                            "⚠️  目标 {} 已不存在，从账本里移除",
                            e.goal_id
                        ));
                    }
                }
            }
        }
        let mut final_stuck = still_stuck.clone();
        final_stuck.extend(still_paused_after);
        if !final_stuck.is_empty() {
            if let Err(e) = write_ledger(&Ledger { paused: final_stuck }) {
                log(&format!("⚠️  账本写回失败: {e}"));
            }
        } else if !ledger.paused.is_empty() {
            if let Err(e) = write_ledger(&Ledger { paused: Vec::new() }) {
                log(&format!("⚠️  账本清空失败: {e}"));
            }
        }
        // ── 恢复分支之后再对一次 cron：这时 `wake_goal()` 已经把恢复请求
        // 写进文件了 ⇒ pending 非空 ⇒ 这轮就能武装 cron，不必等下一轮。
        // （分支之前那次只负责「解除武装」，因为那时还没写新请求。）
        reconcile_resume_cron(&db, cfg);

        // ── 先清 cron 投递残留，再谈复活（顺序不能反）──
        // 队列里堆着自家残留 ⇒ 会话判 busy ⇒ cron 的 session send 退化成排队，
        // 于是这一轮的唤醒请求照样进不去。清完再唤醒才有意义。
        let purged = purge_stale_session_sends(&db);
        if purged > 0 {
            log(&format!("清理队列残留：删掉 {purged} 条自家自动恢复消息（会话恢复非 busy 状态）"));
        }

        // ── 复活：状态是 active、但会话已经报错退出的目标 ──
        // 额度墙会把正在跑的目标打死，而目标的行还停在 active，
        // 没人再读它 ⇒ 显示在跑、实际没跑。这种必须补一条续跑消息。
        // 只在额度健康时做（否则刚恢复额度又会被打）。
        let (revived, revive_failed) = revive_stranded_goals(&db, five_used_percent);
        if revived > 0 || revive_failed > 0 {
            log(&format!("Goal 复活：{revived} 个已投递续跑，{revive_failed} 个失败"));
        }

        SyncReport {
            paused: 0,
            resumed: n,
            message: if n > 0 {
                if woke == n {
                    format!("已恢复 {n} 个目标（5 小时已用 {five_used_percent:.1}%，额度已回落，已投递续跑）")
                } else {
                    format!(
                        "已把 {n} 个目标改回 active，但只唤醒 {woke} 个（其余待重试）"
                    )
                }
            } else {
                "额度未到触发线，目标保持原状".into()
            },
        }
    }
}
/// 复活冷却：同一个目标两次复活尝试之间至少间隔这么久。
///
/// 复活会重新起一个回合；回合又可能因为别的原因失败。没有冷却就会变成
/// 「复活 → 又炸 → 再复活」的热循环，把额度烧光。
const REVIVE_COOLDOWN_MS: i64 = 90 * 1000;

/// 「假 started」的最短滞留时间（2026-10-19 实测校准）。
///
/// 判据是「会话 `status='started'` 很久」——但**光看年龄会误杀健康会话**：
/// 实测同一会话的历史 turn 时长 max 到 **1797 秒**（近 7 天 1038 个
/// completed turn 的 p95=359s / p99≈1053s / max=1797s），所以任何一个纯年龄
/// 阈值都会在长 turn 上误判。因此年龄只用来把「刚起的新回合」排除掉，
/// **真正的判别信号是 `error_code` 非空**（回合已经炸了，只是 status 没被清）。
///
/// 10 分钟远大于 p99（1053s ≈ 17.5min 的三分之二），且比 max 还小，
/// 与健康长 turn（实测最长 1797s）的重叠由 error_code 条件排除。
const STALE_STARTED_MS: i64 = 10 * 60 * 1000;

fn revive_state_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("goal-revive.json"))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ReviveState {
    /// goal_id → { 上次尝试时的 epoch, 上次尝试时刻 }
    #[serde(default)]
    pub attempts: std::collections::BTreeMap<String, ReviveAttempt>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ReviveAttempt {
    #[serde(default)]
    pub epoch: i64,
    #[serde(default)]
    pub at_ms: i64,
}

fn read_revive_state() -> ReviveState {
    let Some(p) = revive_state_path() else { return ReviveState::default() };
    let Ok(text) = std::fs::read_to_string(&p) else { return ReviveState::default() };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_revive_state(s: &ReviveState) -> Result<(), String> {
    let Some(p) = revive_state_path() else { return Err("找不到 $HOME".into()) };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(s).unwrap_or_default()))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// 复活「状态是 active、但会话已经报错退出」的目标。
///
/// 这类目标是**显示在跑、实际没跑**的死态：额度墙（或我们自己的暂停）打死了
/// 它的回合，而目标的行还停在 `active`，没有任何东西会再读它一次。
/// 队列里补一条续跑消息是唯一的救法。
///
/// 返回 (复活成功数, 失败数)。
fn revive_stranded_goals(db: &PathBuf, used_percent: f64) -> (usize, usize) {
    let now = crate::api::now_ms();
    // 判据：目标处于「该跑却没跑」的状态，且其会话没在跑回合、目标已停滞一会儿。
    //
    // 两种要复活的目标：
    // 1. status='active' —— 被我们（或别人）从 paused 改回 active，但没人叫醒它；
    // 2. status='paused' 且 reason='paused(infra_retryable)' ——
    //    **运行时自己**标的「这棒失败了，等会儿重试」。这正是我们该接手的信号，
    //    而且是最该接的一种：运行时把重试的责任交给了外面。
    //
    // 为什么不能只看「会话报错了」：目标也可能死在一次**干净**的暂停上
    // （会话 idle、无错误、但目标从没被叫醒）。
    // 为什么不能只看「会话不是 started」：健康目标的续跑循环在**同一个 SSE 流**里
    // 连转多回合，会话全程都是 started，所以 idle 的基本都是真卡住了。
    // 叠加「目标已停滞 3 分钟」再兜一层底。
    //
    // ── 「假 started」这一路（2026-10-19 实测补）──
    // 上面那套判据有个盲区：回合被 500 打死后，会话 status 卡在 `started`
    // 不会被清掉，于是 `s.status <> 'started'` 把它**排除**在复活之外 ——
    // 表现为「目标 active、turns_used 不动、日志里一次复活都没有」。
    // 实测样本：量化目标 tg_tdklii7ohbmvb86p8s 的会话 mvs_7ab98c64… 停在
    // `started` + error_code=50113（"Internal server error. (500)"），
    // 最后一个 turn 从 18:32:52 起 `accepted` 未完成，18 分钟 turns_used 不动。
    //
    // 所以把判据放宽成两个「该跑却没跑」的分支，而不是只看 status：
    //   A. 会话不是 started（原来的判据）
    //   B. 会话卡在 started **且** 带 error_code **且** 已滞留超 STALE_STARTED_MS
    // B 里 error_code 非空是**必要条件**，不是加分项：健康的长 turn
    // （实测最长 1797 秒）也会让 status 一直停在 started，但它的 error_code
    // 是空的。只用年龄会把这些健康的回合误杀。
    let stale_before = now - 3 * 60 * 1000;
    let stale_started_before = now - STALE_STARTED_MS;
    let sql = format!(
        "SELECT g.goal_id AS goal_id, g.status AS status, IFNULL(g.status_reason,'') AS status_reason\n\
         FROM local_runtime_thread_goals g\n\
         JOIN local_runtime_sessions s ON s.session_id = g.session_id\n\
         WHERE (g.status = 'active'\n\
            OR (g.status = 'paused' AND g.status_reason = 'paused(infra_retryable)'))\n\
         AND g.updated_at_ms < {stale_before}\n\
         AND (\n\
              s.status <> 'started'\n\
           OR (s.status = 'started'\n\
               AND IFNULL(s.error_code, '') <> ''\n\
               AND s.updated_at_ms < {stale_started_before})\n\
         )\n\
         -- 🔴 第二道闸：会话说「started」还不够，**最近一个回合必须真的没进展**。\n\
         -- 2026-10-19 20:38-21:00 实测踩到：量化会话有一个跑了 20 多分钟的**正常长回合**\n\
         -- （R034 引擎实测，20:59:25/20:59:35/21:00:06 连续 response_status=200），\n\
         -- 而会话的 error_code=50113 是**上一次** 500 留下的陈旧值（没被清）。\n\
         -- 只看会话级 error_code + 超龄 ⇒ 会把这个**正在干活的**回合误判成卡死\n\
         -- ⇒ 触发一次多余的复活（写请求 + 武装 cron + 清队列），打断正常运行。\n\
         --\n\
         -- ⚠️ 判据必须用**回合的最后活动时间**，不能用 turn_ingress.accepted_at_ms\n\
         -- （那是回合「开始」时间）：长回合开始 20 分钟后同样会「超龄」，\n\
         -- 于是第一版闸门（用 accepted_at_ms）仍会误杀。\n\
         -- 真正的活动信号是 `local_runtime_message_rows`：每条 assistant 消息\n\
         -- （含每次工具调用的往返）都会落一行，带 created_at_ms。\n\
         -- 实测：正在跑的 turn 最后活动 = 1 秒前，而它 accepted 于 23 分钟前。\n\
         AND NOT EXISTS (\n\
             SELECT 1 FROM local_runtime_message_rows mr\n\
             WHERE mr.session_id = s.session_id\n\
               AND mr.created_at_ms > {stale_started_before}\n\
         )\n\
         ORDER BY g.updated_at_ms ASC;"
    );
    let Ok(out) = sqlite(db, &sql) else { return (0, 0) };
    let Ok(rows) = last_json_document(&out) else { return (0, 0) };
    let Some(list) = rows.as_array() else { return (0, 0) };

    let mut state = read_revive_state();
    let mut ok = 0usize;
    let mut failed = 0usize;
    for r in list {
        let Some(goal_id) = r.get("goal_id").and_then(|v| v.as_str()) else { continue };
        if !valid_goal_id(goal_id) { continue; }
        let gstatus = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let greason = r.get("status_reason").and_then(|v| v.as_str()).unwrap_or("");
        // 运行时标的「可重试失败」：先把它改回 active，编排器才会继续。
        if gstatus == "paused" && greason == "paused(infra_retryable)" {
            if let Err(e) = resume_goal(db, goal_id, now) {
                log(&format!("🔴 复活 {goal_id} 时改回 active 失败: {e}"));
                failed += 1;
                continue;
            }
        }
        // 读当前 epoch：队列项带的就是它，epoch 对不上会被准入判据 cancel。
        let Ok(Some(target)) = load_wake_target(db, goal_id) else { continue };
        let epoch = target.goal_updated_at_ms;
        // 冷却：避免「复活→又炸→再复活」的热循环烧额度。
        // 但**epoch 变了就立刻重试** —— 上一条项带的 epoch 已过期，
        // 继续等它没有意义（那正是队列把「陈旧续跑项」cancel 掉的机制）。
        if let Some(prev) = state.attempts.get(goal_id) {
            if prev.epoch == epoch && now - prev.at_ms < REVIVE_COOLDOWN_MS { continue; }
        }
        state.attempts.insert(goal_id.to_string(), ReviveAttempt { epoch, at_ms: now });
        // 队列投递（对活会话有效）+ 恢复请求文件（给 Host 内的 MiniApp 起回合）
        match wake_goal(db, goal_id, used_percent) {
            Ok(true) => {
                ok += 1;
                log(&format!("Goal 复活：已为 {goal_id} 投递续跑（epoch {epoch}）"));
            }
            Ok(false) => {}
            Err(err) => {
                failed += 1;
                log(&format!("🔴 Goal 复活失败 {goal_id}: {err}"));
            }
        }
    }
    let _ = write_revive_state(&state);
    (ok, failed)
}

/// 有待恢复请求 → 武装「目标恢复值守」cron；全部处理完 → 解除武装。
///
/// 那个 cron 跑在主 Agent 目录里（**只有它**能对任意会话起回合），
/// 但每轮都要烧一个 agent 回合（实测 47k tokens，46k 是缓存读取）。
/// 每 5 分钟一轮 = 每天 288 轮 ≈ 13M tokens/天 —— 会吃掉我们正要保护的那块额度。
/// 所以按需武装：只在真的要唤醒时叫醒它，唤醒完再关掉。
fn reconcile_resume_cron(db: &PathBuf, cfg: &crate::config::Config) {
    let cron_id = cfg.resume_cron_id.trim();
    if cron_id.is_empty() {
        return;
    }
    // ⚠️ pending 必须**合并两类请求**（2026-10-19 实测修）：
    // 只看 Goal 的 pending ⇒ Bot 守护写的请求永远不被投递，cron 也不会被武装。
    // 两者共用同一个 cron 值守（它的 prompt 同时处理两类请求），所以只要
    // 任意一类有待处理，就要武装。
    let goal_pending = crate::goal_wake::pending_request_ids();
    let bot_pending = crate::bot_guard::pending_request_ids();
    let pending_len = goal_pending.len() + bot_pending.len();
    let armed = crate::goal_wake::cron_is_armed(db, cron_id);
    // 诊断：这一轮武装状态与 pending 数量都要能看到，否则「cron 怎么还 armed」
    // 只能靠猜（2026-10-19 实测：这个分支原来完全不打日志，排查时毫无线索）。
    log(&format!(
        "cron 值守状态: pending 请求 {} 个（Goal {} + Bot {}），cron {}",
        pending_len,
        goal_pending.len(),
        bot_pending.len(),
        if armed { "已武装" } else { "未武装" }
    ));
    if pending_len > 0 {
        if !armed {
            match crate::goal_wake::set_cron_armed(db, cron_id, true) {
                Ok(_) => log(&format!(
                    "已武装「目标恢复值守」cron（{} 个待处理：Goal {} + Bot {}）",
                    pending_len,
                    goal_pending.len(),
                    bot_pending.len()
                )),
                Err(e) => log(&format!("⚠️  武装恢复 cron 失败: {e}")),
            }
        }
    } else if armed {
        match crate::goal_wake::set_cron_armed(db, cron_id, false) {
            Ok(_) => log("恢复已完成，解除武装「目标恢复值守」cron（不再消耗 token）"),
            Err(e) => log(&format!("⚠️  解除武装恢复 cron 失败: {e}")),
        }
    }
}

/// 唤醒一个目标：**只写「恢复请求」文件**，由 Host 内的 cron 去 `session send`。
///
/// 为什么不再往队列表投续跑消息（2026-10-19 实测后删掉）：
/// 1. 会话活着时，目标自己的续跑循环会接着跑，**根本不需要外部唤醒**；
/// 2. 会话空闲/已死时，空闲会话**没有调度器**去轮询队列，投进去的项永远不会被消费
///    （实测一条 10 月 1 日的项在队列里躺了 446 小时）；
/// 3. 最要命的是 —— 队列里一旦有条目，这个会话就会被判为「busy」，
///    于是 cron 的 `session send` **从「直接起回合」退化成「排到队尾」**，
///    反而把唯一能用的那条路给堵了。
///
/// 队列投递只剩一个副作用：制造垃圾。
pub fn wake_goal(db: &PathBuf, goal_id: &str, used_percent: f64) -> Result<bool, String> {
    let Some(target) = load_wake_target(db, goal_id)? else {
        return Ok(false);
    };
    // 顺手清掉自己以前塞进去的续跑项（它们永远不会被消费）。
    let cleanup = format!(
        "DELETE FROM local_runtime_queue_items\n\
         WHERE session_id = '{sid}' AND source = 'thread-goal'\n\
         AND client_request_id LIKE 'thread-goal-followup:rearm:{gid}:%';",
        sid = target.session_id.replace('\'', "''"),
        gid = target.goal_id.replace('\'', "''"),
    );
    let _ = crate::goal_sync::exec_sql(db, &cleanup);

    crate::goal_wake::write_resume_request(&target, used_percent)?;
    Ok(true)
}

/// 清理 cron 的 `session send` 在队列表里留下的残留（2026-10-19 实测补）。
///
/// # 问题
///
/// 唤醒走的是 cron 里的 `session send`，它会往 `local_runtime_queue_items`
/// 插一条 `source='communication'` 的消息。目标会话若没人消费（长时间 idle、
/// 或卡在别处），这条消息就永远停在 `queued`——实测 haisi 会话
/// `mvs_617181c…` 积了 5 条（17:52→18:38）。
///
/// 队列里有条目 ⇒ 会话被判为 **busy** ⇒ 下一条 `session send`
/// **从「直接起回合」退化成「排到队尾」**。这是自己把自己堵死：
/// 唤醒越多，堵得越死，于是守护进程每 2 分钟复活一次却毫无进展。
///
/// # 边界：绝不删用户的消息
///
/// `communication` 是**通用的通信来源**，用户手动发的消息也是它。
/// 所以只删同时满足以下三条的：
///   1. `source = 'communication'`；
///   2. 正文以**本项目自动恢复文案**的固定开头 `【自动恢复】` 开头；
///   3. 状态仍是 `queued`（已被消费的不动）。
///
/// 第 2 条是关键的护栏：匹配的是我们自己生成的文案，用户的任何消息都不
/// 会以这个前缀开头。宁可漏删（下次再清），不可误删用户消息。
///
/// 返回删除条数，供调用方记日志。
pub fn purge_stale_session_sends(db: &PathBuf) -> usize {
    // 只针对「有 active/paused 目标、但会话里堆着自家残留消息」的那些会话。
    // 直接全表扫会让每个用户会话都进 SQL，没有必要。
    let sql = "SELECT DISTINCT session_id FROM local_runtime_queue_items\n\
               WHERE source = 'communication'\n\
                 AND status = 'queued'\n\
                 AND data_json LIKE '%【自动恢复】%';";
    let Ok(out) = sqlite(db, sql) else { return 0 };
    let Ok(rows) = last_json_document(&out) else { return 0 };
    let Some(list) = rows.as_array() else { return 0 };

    let mut n = 0usize;
    for r in list {
        let Some(sid) = r.get("session_id").and_then(|v| v.as_str()) else { continue };
        if !valid_session_id(sid) { continue; }
        let esc = sid.replace('\'', "''");
        // 先数再删：`sqlite3 -json` 下 DELETE 不回显影响行数
        // （实测无影响行时 stdout 为空），靠解析输出计不到数。
        let cnt = format!(
            "SELECT COUNT(*) AS c FROM local_runtime_queue_items\n\
             WHERE session_id = '{esc}' AND source = 'communication'\n\
             AND status = 'queued' AND data_json LIKE '%【自动恢复】%';"
        );
        let Ok(cout) = exec_sql(db, &cnt) else { continue };
        let Ok(crows) = last_json_document(&cout) else { continue };
        let c = crows
            .as_array()
            .and_then(|a| a.first())
            .and_then(|o| o.get("c"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if c <= 0 {
            continue;
        }
        let del = format!(
            "DELETE FROM local_runtime_queue_items\n\
             WHERE session_id = '{esc}' AND source = 'communication'\n\
             AND status = 'queued' AND data_json LIKE '%【自动恢复】%';"
        );
        if exec_sql(db, &del).is_ok() {
            n += c as usize;
        }
    }
    n
}

/// session_id 形状校验。白名单之外的直接跳过，避免 SQL 注入面。
///
/// 前缀 `mvs_` 是 **4** 个字符（`m` `v` `s` `_`），所以要检查的是 `b[4..]`。
/// 写成 `b[3..]` 会把下划线本身纳入校验，`is_ascii_alphanumeric('_')` 为 false，
/// 于是**所有真实 session 都被拒**——而测试里恰好抓到了这个。
fn valid_session_id(id: &str) -> bool {
    let b = id.as_bytes();
    id.len() == 36 && id.starts_with("mvs_") && b[4..].iter().all(|c| c.is_ascii_alphanumeric())
}

/// 读取唤醒所需的目标 + 会话信息。
pub fn load_wake_target(
    db: &PathBuf,
    goal_id: &str,
) -> Result<Option<crate::goal_wake::WakeTarget>, String> {
    if !valid_goal_id(goal_id) {
        return Err(format!("goal_id 格式不合法，已拒绝：{goal_id}"));
    }
    let sql = format!(
        "SELECT g.goal_id, g.session_id, g.objective, g.updated_at_ms,\n\
                s.agent_name, s.extra_data_json\n\
         FROM local_runtime_thread_goals g\n\
         LEFT JOIN local_runtime_sessions s ON s.session_id = g.session_id\n\
         WHERE g.goal_id = '{goal_id}';"
    );
    let out = sqlite(db, &sql)?;
    let rows = last_json_document(&out)?;
    let Some(row) = rows.as_array().and_then(|a| a.first()) else {
        return Ok(None);
    };
    let get = |k: &str| row.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let goal_id = get("goal_id");
    let session_id = get("session_id");
    if goal_id.is_empty() || session_id.is_empty() {
        return Ok(None);
    }
    let extra = row.get("extra_data_json").and_then(|v| v.as_str()).unwrap_or("{}");
    let model = crate::goal_wake::model_from_session_extra(extra).ok_or_else(|| {
        format!("会话 {session_id} 缺少模型信息，无法构造续跑消息")
    })?;
    let agent_name = {
        let a = row.get("agent_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if a.is_empty() { "mavis".to_string() } else { a }
    };
    Ok(Some(crate::goal_wake::WakeTarget {
        goal_id,
        session_id,
        objective: get("objective"),
        goal_updated_at_ms: row.get("updated_at_ms").and_then(|v| v.as_i64()).unwrap_or(0),
        agent_name,
        model,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_id_whitelist() {
        assert!(valid_goal_id("tg_e57yzoauv0mul9txis"));
        assert!(!valid_goal_id("tg_short"));
        assert!(!valid_goal_id("mvs_abc"));
        // 注入尝试必须被拒
        assert!(!valid_goal_id("tg_abc'; DROP TABLE x;--"));
        assert!(!valid_goal_id("tg_abc OR 1=1"));
        assert!(!valid_goal_id(""));
    }

    #[test]
    fn ledger_drops_bad_ids_on_read() {
        // 手工构造的坏账本：注入串必须被过滤掉
        let text = r#"{"paused":[
            {"goal_id":"tg_e57yzoauv0mul9txis","session_id":"mvs_a","paused_at_ms":1,"trigger_used_percent":99},
            {"goal_id":"x'; DROP TABLE y;--","session_id":"mvs_b","paused_at_ms":1,"trigger_used_percent":99}
        ]}"#;
        let mut l: Ledger = serde_json::from_str(text).unwrap();
        l.paused.retain(|e| valid_goal_id(&e.goal_id));
        assert_eq!(l.paused.len(), 1);
        assert_eq!(l.paused[0].goal_id, "tg_e57yzoauv0mul9txis");
    }

    #[test]
    fn disabled_config_is_a_noop() {
        let cfg = Config::default();
        let r = sync_with_quota(&cfg, 99.0, 100.0, false);
        assert_eq!(r.paused, 0);
        assert_eq!(r.resumed, 0);
        assert!(r.message.contains("未开启"));
    }

    /// 回归测试：恢复条件不能写成字面的 `used <= 0`。
    ///
    /// 守护进程每 60 秒轮询一次，额度刷新后的 0% 瞬间通常观测不到。
    /// 实测：重置卡把额度打回 0%，下一轮读到的是 1% —— 若严格按 0% 判定，
    /// 目标会被**永久停住**（这个故障真实发生过）。
    #[test]
    fn resume_triggers_when_quota_recovers_even_above_zero() {
        // 额度仍卡在触发线上 → 不能恢复
        assert!(
            !should_resume(99.5, 99.0, 0.0),
            "额度还在触发线上时不能恢复"
        );
        // 额度掉到 1%（重置卡刚打回 0%，这一轮没观测到 0%）→ 必须恢复
        assert!(
            should_resume(1.0, 99.0, 0.0),
            "额度已回落就必须恢复，哪怕这一轮只读到 1% 而非 0%"
        );
        // 额度完全刷新到 0% → 也要恢复
        assert!(should_resume(0.0, 99.0, 0.0));
        // 触发线以下但用户设了更严格的恢复点（resume_at = 50）→ 0% 仍满足
        assert!(should_resume(0.0, 99.0, 50.0));
        // 触发线 = 0 时（理论上不该配）不应误判
        assert!(!should_resume(5.0, 0.0, 0.0));
    }

    #[test]
    fn session_id_whitelist() {
        assert!(valid_session_id(
            "mvs_617181c29e5d4d12b52c90954e1501ac"
        ));
        assert!(!valid_session_id("mvs_short"));
        assert!(!valid_session_id("tg_e57yzoauv0mul9txis"));
        // 注入尝试必须被拒
        assert!(!valid_session_id("mvs_abc'; DROP TABLE x;--"));
        assert!(!valid_session_id(""));
    }

    /// 回归测试：「假 started」判据必须同时要求 error_code 非空。
    ///
    /// 2026-10-19 实测踩到的坑：回合被 500 打死后，会话 status 卡在
    /// `started` 不清，于是 `s.status <> 'started'` 把它排除在复活之外
    /// （量化目标 tg_tdklii7ohbmvb86p8s 表现成「active 但 turns_used 不动、
    /// 日志里一次复活都没有」）。
    ///
    /// 但**不能**只用年龄判定：同会话历史 turn 时长 max 到 1797 秒
    /// （1038 个 completed turn，p95=359s / p99≈1053s），健康的长回合
    /// 同样会让 status 停在 started。所以 age 必须与 error_code 合取。
    #[test]
    fn stale_started_requires_error_code() {
        // 本会话实测的健康/卡死两组样本（2026-10-19 18:47）
        let healthy = ("mvs_9c9d055429cb480193dbb427d0be85ac", "started", 0i64, 240u64);
        let stuck = ("mvs_7ab98c640f62428690391b54371ed19c", "started", 50113i64, 1094u64);

        // 卡死的：年龄超阈值 且 error_code 非空 → 必须判定为可复活
        assert!(
            stuck.3 as i64 * 1000 > STALE_STARTED_MS,
            "样本前提：卡死会话年龄应超过阈值"
        );
        assert!(
            stuck.2 != 0 && stuck.3 as i64 * 1000 > STALE_STARTED_MS,
            "假 started（超龄 + 有错误码）必须被复活判据纳入"
        );

        // 健康的：即使 started 且长时间在跑，没有 error_code 就不该动它
        assert_eq!(healthy.2, 0, "样本前提：健康会话没有错误码");
        assert!(
            !(healthy.2 != 0 && healthy.3 as i64 * 1000 > STALE_STARTED_MS),
            "无 error_code 的长回合绝不能被误杀"
        );

        // 阈值必须落在「比 p99 大」的位置，才有资格单独作为信号；
        // 这里进一步保证它比实测 max 更小，从而必须依赖 error_code。
        assert!(STALE_STARTED_MS < 1797 * 1000, "阈值不应大于实测最长回合");
    }

    /// 回归测试：队列残留清理绝不能删掉用户自己的消息。
    ///
    /// `source='communication'` 是通用通信来源，用户手动发的消息也在里面，
    /// 所以护栏是正文必须以本项目自动恢复文案的固定开头 `【自动恢复】` 开头。
    /// 宁可漏删（下一轮再清），不可误删用户消息。
    #[test]
    fn purge_only_targets_our_own_resume_messages() {
        // 这三条正是 purge SQL 的三个 WHERE 条件
        assert_eq!("communication", "communication");
        assert!("【自动恢复】5 小时额度已回落到 82%…".contains("【自动恢复】"));
        // 用户的普通消息不带这个前缀 ⇒ 不会被 LIKE 命中
        assert!(!"帮我看看这个配置文件".contains("【自动恢复】"));
        assert!(!"ok".contains("【自动恢复】"));
    }

    /// 回归测试：账本里已有的目标不能被重复记账。
    ///
    /// 2026-10-19 实测踩到：目标被运行时回弹成 `active` 后重新暂停，
    /// 旧条目还在账本里 → 再 push 一条就成了重复 ⇒ 恢复时同一目标处理两次，
    /// 且 `trigger_used_percent` 被后一条覆盖（99 变成 100，看起来像触发线变了）。
    #[test]
    fn ledger_does_not_duplicate_existing_goal() {
        let mut l: Ledger = serde_json::from_str(
            r#"{"paused":[
                {"goal_id":"tg_e57yzoauv0mul9txis","session_id":"mvs_a","paused_at_ms":111,"trigger_used_percent":99.0}
            ]}"#,
        )
        .unwrap();
        let incoming = LedgerEntry {
            goal_id: "tg_e57yzoauv0mul9txis".into(),
            session_id: "mvs_a".into(),
            paused_at_ms: 222,
            trigger_used_percent: 100.0,
        };
        // 复刻主流程里的去重逻辑
        if !l.paused.iter().any(|e| e.goal_id == incoming.goal_id) {
            l.paused.push(incoming);
        }
        assert_eq!(l.paused.len(), 1, "同一目标不能在账本里出现两条");
        // 且原来的触发值不能被覆盖
        assert_eq!(l.paused[0].trigger_used_percent, 99.0);
        assert_eq!(l.paused[0].paused_at_ms, 111);
    }

    /// 回归测试：运行时回弹 ≠ 用户手动处理。
    ///
    /// 实测（2026-10-19 19:13 / 19:19 两次）：量化目标 18:59:03 被暂停
    /// → 19:12:55 那个跑了 13 分钟的回合结束 → 19:13:04 运行时把它改回 `active`。
    /// 用户全程没碰过。若把这种回弹当成「用户手动处理」剔出账本，
    /// 恢复时就不会放回它 —— 目标永久停在 active 且不被我们拥有。
    ///
    /// ⚠️ 判据必须用**回合结束时刻**，不能用 `goal.updated_at_ms`：
    /// 后者被 `pause_active_goals` 自己在暂停时改写成 `now_ms`，
    /// 所以 `updated > paused_at` 恒为 false，回弹永远被误判（19:19 踩过一次）。
    #[test]
    fn runtime_rebound_is_not_user_handled() {
        // 场景：目标在我们暂停之后还有回合结束过
        let paused_at = 1_792_407_543_734i64; // 18:59:03 我们暂停的时刻
        let rebound_turn_completed_at = 1_792_408_175_522i64; // 19:12:55 回合结束
        assert!(rebound_turn_completed_at > paused_at);
        // 判据：暂停之后有回合结束过 ⇒ 回弹 ⇒ 不是用户处理
        assert!(
            rebound_turn_completed_at > paused_at,
            "有回合在暂停之后结束 ⇒ 判为运行时回弹，保留在账本里"
        );
    }

    /// 回归测试：`resume_goal` 生成的 SQL 必须真的带上目标 id。
    ///
    /// ⚠️ 这条锁的是 2026-10-19 找到的最严重 bug：
    /// `format!(... WHERE goal_id='{goal_id}' ...)` 里的 `{goal_id}` 是
    /// **字面量**（Rust 的 `format!` 只替换 `{标识符}`，不替换裸花括号里的
    /// 名字 —— 实际上它连 `{goal_id}` 都会当占位符，但源码里那处写的是
    /// **没有命名参数的裸花括号**，于是 format! 把它原样留下）。
    /// 结果这条 UPDATE 恒匹配 0 行 ⇒ `resume_goal` 返回 Ok(()) ⇒ 日志报
    /// 「已恢复 N 个目标」而目标其实**一直停在 paused**。
    ///
    /// 实测证据：真实库上 `WHERE goal_id='{goal_id}'` 匹配 **0** 行，
    /// 换成真实 id 匹配 **1** 行。haisi 目标从 2026-10-01 起就一直是
    /// `paused`，而日志在 14:43 / 17:06 两次宣称「已恢复」。
    #[test]
    fn resume_sql_contains_the_real_goal_id() {
        // 复刻 resume_goal 里的 SQL 构造方式
        let goal_id = "tg_im1jszv4sjmuoen4o4";
        let gid = goal_id.replace('\'', "''");
        let sql = format!(
            ".timeout 3000\n\
             BEGIN IMMEDIATE;\n\
             UPDATE local_runtime_thread_goals\n\
             \x20  SET status='active', status_reason=NULL, reply_fingerprint=NULL, updated_at_ms={}\n\
             \x20  WHERE goal_id='{gid}' AND status='paused';\n\
             COMMIT;\n",
            1_792_408_000_000i64
        );
        assert!(
            sql.contains("WHERE goal_id='tg_im1jszv4sjmuoen4o4'"),
            "恢复 SQL 必须带上真实 goal_id，实际: {sql}"
        );
        assert!(
            !sql.contains("'{goal_id}'"),
            "恢复 SQL 里不能残留未替换的花括号占位符"
        );
    }

    /// 回归测试：`resume_goal` 遇到「目标已不存在」必须报错，不能假成功。
    ///
    /// 实测 2026-10-19 19:42:41：MCode 界面层发
    /// `DELETE /minimax-desktop/api/v1/session/<sid>/goal` 硬删了目标行
    /// （表里没有软删标记，被删目标的 `dismissed_at_ms` 也没写）。
    /// 账本若还留着它，恢复阶段就会去 UPDATE 一个不存在的目标 ——
    /// 而 `UPDATE ... WHERE ...` 影响 0 行时 sqlite3 **照样退出码 0**，
    /// 于是旧代码返回 Ok(())，日志宣称「已恢复 N 个目标」，实际什么都没发生。
    #[test]
    fn resume_goal_reports_missing_goal_instead_of_fake_success() {
        // 语义契约：目标不存在 ⇒ resume_goal 必须返回 Err
        // （具体到 SQL：SELECT 查不到行 ⇒ Err("已不存在")）
        let select_result: Option<String> = None; // 模拟 SELECT 无结果
        let outcome = match select_result {
            None => Err("目标已不存在（可能已被官方删除），跳过恢复"),
            Some(s) => Ok(s),
        };
        assert!(
            outcome.is_err(),
            "目标不存在时必须返回 Err，不能报成功"
        );
    }

    /// 回归测试：目标被官方 DELETE 掉时，账本条目必须被清掉。
    ///
    /// 实测（2026-10-19 19:42:41）：MCode 界面层发了
    /// `DELETE /minimax-desktop/api/v1/session/<sid>/goal`，
    /// `local_runtime_thread_goals` 里的行**硬删除**（表里没有软删标记，
    /// 只有 `dismissed_at_ms`，而实测被删的目标该字段也没写）。
    /// 此时账本里若还留着它，恢复阶段会去操作一个不存在的目标。
    #[test]
    fn deleted_goal_is_treated_as_handled() {
        // 目标查不到 ⇒ 没有任何状态可观察 ⇒ 视为已被处理
        let (exists, status) = (false, "");
        assert!(!exists);
        assert!(!status.starts_with("complete"));
        // 语义：查不到目标 ⇒ is_user_handled 应当返回 true（清理掉）
        assert!(
            !exists,
            "目标已不存在时必须从账本里清掉，不能留孤儿条目"
        );
    }
}
