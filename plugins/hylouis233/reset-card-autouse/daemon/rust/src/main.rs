//! 后台守护进程：额度探测 + 先过期先用决策 + 自动扣卡。
//!
//! 与 Mini App「蹬王之王」的判定逻辑严格同构，由 `verify-parity.mjs`
//! 逐字比对「动作 + 理由文本」防止漂移。
//!
//! 设计要点：
//!   - 分层缓存：额度每轮取，积分/卡/汇总按各自变化速度缓存
//!   - 先记账再请求：崩溃或重启都不会重复扣同一张卡
//!   - 幂等键：用服务端下发的 out_biz_no，重试安全

mod api;
mod config;
mod decision;
mod bot_bootstrap;
mod bot_guard;
mod goal_sync;
mod goal_wake;
mod state;

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use api::{Client, ApiError, BASE_CN};
use decision::{
    amount, decide, fmt_day, parse_quotas, Action, DecisionCfg, QuotaState, RemainsPayload,
    UsableCard, QUOTA_FIVE_HOUR,
};
use serde_json::json;

/// 「投递空转」的判定门槛：待唤醒请求的积压条数连续多少轮没变就告警。
///
/// 取 15 轮：巡检间隔 60 秒 ⇒ 约 15 分钟。
/// ⚠️ 不能更短 —— 一轮唤醒要给每个 bot 起一个 agent 回合，
/// 实测一批要跑几分钟，期间积压条数确实不会变（成功一个才 ack 一个）。
const STUCK_STREAK_ROUNDS: u32 = 15;

/// 把 Unix 毫秒格式化成「10月19日 19:41」这样的本地时间。
///
/// 用来在日志里写明「5 小时窗口什么时候刷新」——
/// 之前没有这一项，导致窗口刷新时间只能靠猜
/// （2026-10-19 实测：以为 19:41 会刷新，实际到 19:54 仍是 100%）。
fn fmt_reset_at(ms: i64) -> String {
    if ms == 0 {
        return "未知".to_string();
    }
    let secs = ms.div_euclid(1000);
    let (m, d) = decision::local_md(secs);
    let (h, mi) = decision::local_hm(secs);
    if m == 0 {
        return "未知".to_string();
    }
    format!("{m}月{d}日 {h:02}:{mi:02}")
}

/// 日志：带本地时间戳，便于 tail 追踪。
pub fn log(msg: &str) {
    use std::io::Write;
    let now = api::now_ms();
    let secs = now.div_euclid(1000);
    let ms = now.rem_euclid(1000);
    // 复用 decision 里的本地时区换算
    let (month, day) = decision::local_md(secs);
    let (hh, mm) = decision::local_hm(secs);
    let _ = std::io::stdout().write_all(
        format!("[{month:02}-{day:02} {hh:02}:{mm:02}.{ms:03}] {msg}\n").as_bytes(),
    );
}

/// 各数据源的缓存 TTL（毫秒）。按变化速度分配，不是所有数据都值得每轮取。
struct Ttl {
    quota: i64,
    code_credits: i64,
    cards: i64,
    platform: i64,
}

impl Default for Ttl {
    fn default() -> Self {
        // quota TTL=0：额度是所有联动的触发源，必须每轮看。
        // 其余按「多久会变」给：积分随消耗变、卡按天签到变、平台池极少变。
        Ttl { quota: 0, code_credits: 60_000, cards: 300_000, platform: 300_000 }
    }
}

struct Cache<T: Clone> {
    slots: BTreeMap<&'static str, (Option<T>, i64)>,
}

impl<T: Clone> Cache<T> {
    fn new() -> Self {
        Cache { slots: BTreeMap::new() }
    }
    fn get(&mut self, key: &'static str, ttl: i64) -> Option<T> {
        let entry = self.slots.get(key).cloned();
        if let Some((Some(v), at)) = entry {
            if api::now_ms() - at < ttl {
                return Some(v);
            }
        }
        None
    }
    fn put(&mut self, key: &'static str, value: T) {
        self.slots.insert(key, (Some(value), api::now_ms()));
    }
    fn invalidate(&mut self, key: &str) {
        self.slots.remove(key);
    }
}

struct Caches {
    quota: Cache<serde_json::Value>,
    code: Cache<serde_json::Value>,
    platform: Cache<serde_json::Value>,
    cards: Cache<Vec<serde_json::Value>>,
}

impl Caches {
    fn new() -> Self {
        Caches {
            quota: Cache::new(),
            code: Cache::new(),
            platform: Cache::new(),
            cards: Cache::new(),
        }
    }
}

struct Daemon {
    client: Client,
    ttl: Ttl,
    cache: Caches,
    /// 上次报告过的「待唤醒请求积压」条数。
    ///
    /// 用来**只告警一次**（不刷屏）：积压条数增加 ⇒ 状态又变了，值得再说一句；
    /// 条数没变 ⇒ 上一条告警仍然有效，不必重复。
    last_reported_pending: usize,
    /// 「投递空转」观测器：运行时在跑但积压条数一直不减少。
    stuck: bot_bootstrap::StuckWatch,
    /// 上次报告过的自检错误（同一错误只说一次，避免每 60 秒刷一行）。
    last_selfcheck_error: Option<String>,
}

/// 取缓存：命中则零网络，未命中才请求。
///
/// 做成自由函数（而非 `&mut self` 方法）：这样 `cache` 与 `client`
/// 可以分别借用，不会触发「同一个 self 被可变借两次」。
fn cached(
    client: &mut Client,
    cache: &mut Cache<serde_json::Value>,
    key: &'static str,
    ttl: i64,
    load: impl FnOnce(&mut Client) -> Result<serde_json::Value, ApiError>,
) -> Result<serde_json::Value, ApiError> {
    if let Some(v) = cache.get(key, ttl) {
        return Ok(v);
    }
    let v = load(client)?;
    cache.put(key, v.clone());
    Ok(v)
}

impl Daemon {
    fn new(client: Client) -> Self {
        Daemon {
            client,
            ttl: Ttl::default(),
            cache: Caches::new(),
            last_reported_pending: 0,
            stuck: bot_bootstrap::StuckWatch::new(),
            last_selfcheck_error: None,
        }
    }

    /// 读额度。**读不到就如实报错，绝不返回空数组冒充「额度充足」。**
    ///
    /// 旧实现在失败时返回 `vec![]`，而 `decide()` 对空额度会判成
    /// 「额度充足（已用 0%）」——于是 API 一挂，日志照样显示一切正常，
    /// Goal 同步也被 `find()` 静默跳过，整个功能无声失效。
    fn quotas(&mut self) -> Result<Vec<QuotaState>, ApiError> {
        let ttl = self.ttl.quota;
        let Daemon { client, cache, .. } = self;
        let v = cached(client, &mut cache.quota, "quota", ttl, |c| {
            c.get("/backend/account/token_plan/remains_percent", false)
        })?;
        let payload: RemainsPayload = serde_json::from_value(v).unwrap_or(RemainsPayload {
            model_remains: vec![],
        });
        let qs = parse_quotas(&payload);
        if qs.is_empty() {
            // 响应本身是 200，但里面没有可解析的额度项 —— 同样按失败处理，
            // 否则「空额度」会被后续逻辑当成「额度充足」。
            return Err(ApiError::Business(
                "额度接口返回了空列表，无法判断 5 小时额度".into(),
            ));
        }
        Ok(qs)
    }

    /// 全部分页拉取重置卡
    fn all_cards(&mut self) -> Vec<serde_json::Value> {
        let ttl = self.ttl.cards;
        if let Some(v) = self.cache.cards.get("cards", ttl) {
            return v;
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut all = Vec::new();
        for page in 1..=10 {
            match self
                .client
                .get(&format!("/backend/account/reset_card/list?page={page}&size=20"), false)
            {
                Ok(v) => {
                    let cards = v.get("cards").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                    if cards.is_empty() {
                        break;
                    }
                    let n = cards.len();
                    for c in cards {
                        if let Some(id) = c.get("card_id").and_then(|x| x.as_str()) {
                            if seen.insert(id.to_string()) {
                                all.push(c);
                            }
                        }
                    }
                    if n < 20 {
                        break;
                    }
                }
                Err(e) => {
                    log(&format!("  重置卡列表读取失败（忽略）: {e}"));
                    break;
                }
            }
        }
        self.cache.cards.put("cards", all.clone());
        all
    }

    /// 可用于扣卡的卡：未过期 + 有 out_biz_no + 有 source_type，按到期排序
    fn usable_cards(&mut self) -> Vec<UsableCard> {
        let now = api::now_ms();
        let mut out: Vec<UsableCard> = self
            .all_cards()
            .iter()
            .filter_map(|c| {
                let expire = c.get("expire_time").and_then(|v| v.as_i64())?;
                if expire <= now {
                    return None;
                }
                let out_biz_no = c.get("out_biz_no").and_then(|v| v.as_str()).unwrap_or("");
                let source_type = c.get("source_type").and_then(|v| v.as_str()).unwrap_or("");
                if out_biz_no.is_empty() || source_type.is_empty() {
                    return None;
                }
                Some(UsableCard {
                    card_id: c.get("card_id")?.as_str()?.to_string(),
                    source_type: source_type.to_string(),
                    out_biz_no: out_biz_no.to_string(),
                    expire_time: expire,
                })
            })
            .collect();
        out.sort_by_key(|c| c.expire_time);
        out
    }

    /// 全账户里最早到期的一笔「仍有效且有余额」的积分（两个来源取更早的）
    fn earliest_credit(&mut self) -> (Option<i64>, String, bool) {
        let now = api::now_ms();
        let mut best: Option<i64> = None;
        let mut desc = String::new();
        // 积分数据是否**完整读到**。
        // ⚠️ 2026-10-19 PR #28 review 修：读不到 ≠ 没有。
        // 以前两个来源读失败只是记日志（`best` 保持 None），于是
        // `decide` 收到 `credit_expiry = None` ⇒ 直接走「没有积分 ⇒ 用卡」。
        // 扣卡是真实写操作，网络故障不该导致误扣 ⇒ 任一来源读失败即视为不完整。
        let mut complete = true;

        let mut consider = |exp: Option<i64>, balance: f64, label: &str| {
            let (Some(e), true) = (exp, balance > 0.0) else { return };
            if e <= now {
                return;
            }
            if best.is_none_or(|b| e < b) {
                best = Some(e);
                desc = label.to_string();
            }
        };

        // 平台积分池
        let pttl = self.ttl.platform;
        let platform_result = {
            let Daemon { client, cache, .. } = self;
            cached(client, &mut cache.platform, "platform", pttl, |c| {
                c.get("/backend/account/token_plan_credit", false)
            })
        };
        match platform_result {
            Ok(v) => {
                if let Some(buckets) = v
                    .get("balance_breakdown")
                    .and_then(|b| b.get("buckets"))
                    .and_then(|b| b.as_array())
                {
                    for b in buckets {
                        let label = match b.get("source").and_then(|s| s.as_i64()) {
                            Some(2) => "平台积分·充值".to_string(),
                            Some(3) => "平台积分·赠送".to_string(),
                            _ => "平台积分·未知".to_string(),
                        };
                        consider(
                            b.get("expire_time_ms").and_then(|v| v.as_i64()),
                            amount(b.get("balance").unwrap_or(&json!(0))),
                            &label,
                        );
                    }
                }
            }
            Err(e) => {
                complete = false;
                log(&format!("  平台积分读取失败（本轮不做写操作）: {e}"))
            }
        }

        // Code 积分明细
        let cttl = self.ttl.code_credits;
        let raw = {
            let Daemon { client, cache, .. } = self;
            cached(client, &mut cache.code, "code", cttl, |c| {
                c.get(
                    "/minimax-cloud/api/v1/credit/details?page_number=1&page_size=100&include_expired=1",
                    true,
                )
            })
        };
        match raw {
            Ok(v) => {
                if let Some(details) = v.get("details").and_then(|d| d.as_array()) {
                    for item in details {
                        let label = if item.get("credit_type").and_then(|c| c.as_i64()) == Some(2) {
                            "Code 积分·赠送"
                        } else {
                            "Code 积分·购买"
                        };
                        consider(
                            item.get("expire_at_ms").and_then(|v| v.as_i64()),
                            amount(item.get("remaining_amount").unwrap_or(&json!(0))),
                            label,
                        );
                    }
                }
            }
            Err(e) => {
                complete = false;
                log(&format!("  Code 积分读取失败（本轮不做写操作）: {e}"))
            }
        }

        (best, desc, complete)
    }

    /// 一轮：判定 + 必要时扣卡
    fn tick(&mut self, dry_run: bool, cfg: &config::Config) -> Result<(Action, String, bool), ApiError> {
        // 额度读不到 = 不知道现在该不该停目标、该不该补卡。
        // 此时**任何动作都是瞎猜**，所以整轮中止，绝不按「额度充足」往下走。
        let quotas = match self.quotas() {
            Ok(q) => q,
            Err(e) => {
                log(&format!(
                    "⛔ 额度读取失败，本轮中止（未做任何暂停/恢复/扣卡）: {e}"
                ));
                return Err(e);
            }
        };
        let usable = self.usable_cards();
        let (credit_expiry, credit_desc, credit_data_complete) = self.earliest_credit();

        // ── 第 0 步：守护 MiniMax Bot 团队（失败绝不拖垮任何东西）──
        // 与 Goal 暂停/恢复互不依赖：bot 挂掉和额度是两回事。
        if cfg.bot_guard.enabled {
            match bot_guard::tick(&cfg.bot_guard) {
                Ok((checked, self_healing, needs_user, revived)) => {
                    if needs_user > 0 || self_healing > 0 {
                        log(&format!(
                            "Bot 看门狗：检查 {checked} 个，待人工 {needs_user} 个，可自愈 {self_healing} 个，本轮投递 {revived} 个"
                        ));
                    }
                }
                Err(e) => log(&format!("⚠️  Bot 守护本轮出错（不影响其它功能）: {e}")),
            }
        }

        // ── 第 0.5 步：唤醒链路自检 ──
        //
        // 唤醒的执行方是蹬王之王自己的 MiniApp（它有 agentAccess，能起回合）。
        // 但 MiniApp 是 on-demand 的，**宿主重启后不会自动拉起它** ⇒
        // 那段时间里「判定」照常写请求、「投递」却没人消费。
        //
        // ⚠️ 这里**不做自动修复** —— 2026-10-20 逐条实测过，从宿主外拉起 MiniApp
        // 的每一条路都堵死（队列写入不被消费 / loopback 凭据只在宿主进程 /
        // 宿主 HTTP 无此接口 / deeplink 只导航页面 / 官方插件不可改版）。
        // 详见 `bot_bootstrap.rs` 的完整对照表。
        //
        // ⇒ 这里只做**如实告警**：宁可吵，也不能静默地一个 bot 都没叫醒。
        if cfg.bot_guard.enabled {
            let pending = bot_guard::pending_request_ids().len();
            match bot_bootstrap::check(pending) {
                bot_bootstrap::SelfCheck::NeedsAttention { pending } => {
                    // 只在「从有到无」的那一刻说一次，避免每分钟刷屏。
                    if pending > self.last_reported_pending {
                        log(&format!(
                            "⚠️  Bot 看门狗：{pending} 个待唤醒请求投不出去 —— 蹬王之王面板的 MiniApp 运行时没在跑（多半是宿主重启后没被拉起）。请打开一次「蹬王之王」面板，它一恢复就会把积压的请求全部补上。"
                        ));
                    }
                    self.last_reported_pending = pending;
                    // 运行时没起时，「投递空转」是已知原因 ⇒ 观察器复位，不重复报。
                    self.stuck.observe(0, STUCK_STREAK_ROUNDS);
                }
                bot_bootstrap::SelfCheck::Delivering { pending } => {
                    // 运行时在跑 ⇒ 恢复提示是准确的。
                    if self.last_reported_pending > 0 {
                        log("Bot 看门狗：运行时已恢复，积压的唤醒请求正在补上");
                    }
                    self.last_reported_pending = 0;
                    // 接着看「运行时在跑但请求没被消费」的空转情形。
                    match self.stuck.observe(pending, STUCK_STREAK_ROUNDS) {
                        bot_bootstrap::StuckEvent::None | bot_bootstrap::StuckEvent::Started => {}
                        bot_bootstrap::StuckEvent::Recovered => {
                            log("Bot 看门狗：待唤醒请求的积压条数在变化，唤醒链路已恢复消费");
                        }
                        bot_bootstrap::StuckEvent::Stuck { pending, rounds } => {
                            log(&format!(
                                "⚠️  Bot 看门狗：运行时在跑，但 {pending} 个待唤醒请求已连续 {rounds} 轮巡检（约 {rounds} 分钟）没被消费 —— 唤醒循环可能已经空转。请打开「蹬王之王」面板看唤醒日志。"
                            ));
                        }
                    }
                }
                bot_bootstrap::SelfCheck::Idle => {
                    // ⚠️ 这里**不能**说「运行时已恢复」——
                    // pending 归零只说明「没有待办」，不代表运行时起来了。
                    if self.last_reported_pending > 0 {
                        log("Bot 看门狗：积压的待唤醒请求已清空");
                    }
                    self.last_reported_pending = 0;
                    self.stuck.observe(0, STUCK_STREAK_ROUNDS);
                }
                bot_bootstrap::SelfCheck::Failed(e) => {
                    // ⚠️ **只说一次**（2026-10-20 独立审计 F3）：
                    // 自检失败是「判据失效」，可能持续很久
                    // （比如 lsof 一直报错）⇒ 不去重就是每天 1440 行噪音。
                    if self.last_selfcheck_error.as_deref() != Some(e.as_str()) {
                        self.last_selfcheck_error = Some(e.clone());
                        log(&format!("⚠️  Bot 看门狗自检失败（不影响其它功能）: {e}"));
                    }
                }
            }
        }

        // ── 第 1 步：同步 Goal（失败不拖垮扣卡）──
        // 顺序固定为「先让目标停下，再决定要不要补额度」，反过来两者会打架。
        if cfg.goal_pause.enabled {
            match quotas.iter().find(|q| q.quota_type == QUOTA_FIVE_HOUR) {
                Some(five) => {
                    let r = goal_sync::sync_with_quota(
                        cfg,
                        five.used,
                        five.total,
                        five.is_unlimited,
                    );
                    let tag = if r.paused > 0 {
                        "Goal 暂停"
                    } else if r.resumed > 0 {
                        "Goal 恢复"
                    } else {
                        "Goal"
                    };
                    log(&format!("{tag}: {}", r.message));
                }
                // 以前这里是静默跳过：开关开着、额度列表里却没有 5 小时项，
                // 目标该停不停，日志一个字都没有。现在必须喊出来。
                None => log("⚠️  Goal 自动暂停已开启，但本轮额度里没有「5 小时限额」，本轮未做暂停/恢复"),
            }
        }

        let d = decide(
            &quotas,
            &usable,
            credit_expiry,
            &credit_desc,
            &cfg.decision_cfg(),
            api::now_ms(),
            credit_data_complete,
        );
        log(&format!(
            "额度: {}",
            quotas.iter().map(|q| q.describe()).collect::<Vec<_>>().join(" | ")
        ));
        // 距 5 小时窗口刷新还有多久 —— 这决定了「还能不能再等到额度自己回来」。
        // 之前日志里没有这一项，导致「窗口到底什么时候刷新」只能靠猜
        // （2026-10-19 实测：以为 19:41 会刷新，实际到 19:54 仍是 100%）。
        if let Some(five) = quotas.iter().find(|q| q.quota_type == decision::QUOTA_FIVE_HOUR) {
            match decision::minutes_to_reset(Some(five), api::now_ms()) {
                Some(m) => log(&format!(
                    "5 小时窗口: {m:.0} 分钟后刷新（预计 {}）",
                    fmt_reset_at(five.reset_time_ms.unwrap_or(0))
                )),
                None => log("5 小时窗口: 额度接口未提供刷新时间"),
            }
        }
        log(&format!("决策[{}]: {}", d.action.as_str(), d.reason));

        if d.action != Action::UseCard || dry_run {
            return Ok((d.action, d.reason, false));
        }
        let Some(card) = usable.first() else {
            return Ok((d.action, d.reason, false));
        };

        let fingerprint = format!("{}:{}", card.card_id, card.out_biz_no);
        if state::was_spent(&fingerprint) {
            log("这张卡本进程已尝试过，跳过");
            return Ok((d.action, d.reason, false));
        }

        // 先记账再请求：崩溃/重启都不会重复扣
        state::mark_spent(&fingerprint, &card.card_id, &card.source_type);
        self.cache.cards.invalidate("cards");

        match self
            .client
            .post(
                "/backend/account/reset_card/use",
                &json!({ "source_type": card.source_type, "out_biz_no": card.out_biz_no }),
                false,
            ) {
            Ok(v) => {
                let outcome = v.get("outcome").and_then(|x| x.as_str()).unwrap_or("UNKNOWN");
                let ok = outcome == "SUCCESS" || outcome == "REPLAYED";
                if ok {
                    log(&format!("✓ 已使用重置卡 {}，额度已重置", card.card_id));
                } else {
                    log(&format!("✗ 重置卡不可用（{outcome}）"));
                }
                Ok((d.action, d.reason, ok))
            }
            Err(e) => {
                log(&format!("✗ 扣卡失败：{e}"));
                Ok((d.action, d.reason, false))
            }
        }
    }
}

/// 校验模式：读入与 Node/Python 同一份用例 JSON，打印同样的结果结构。
///
/// 供 `verify-parity.mjs` 调用，让三方（Mini App / Python / Rust）跑同一组输入。
/// 不联网、不落盘，纯函数比对。
fn selftest() {
    use std::io::Read as _;
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("selftest: 无法读取 stdin");
        std::process::exit(2);
    }
    let payload: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("selftest: JSON 解析失败 {e}");
            std::process::exit(2);
        }
    };
    let now = payload["now"].as_i64().unwrap_or(0);
    let day = payload["day"].as_i64().unwrap_or(86_400_000);
    // 三方必须喂同一个阈值，否则比的不是同一件事
    let trigger = payload["trigger"].as_f64().unwrap_or(99.0);
    let tight = payload["tight"].as_f64().unwrap_or(80.0);
    let low = payload["low"].as_f64().unwrap_or(95.0);
    let skip = payload["skipBeforeResetMinutes"].as_f64().unwrap_or(60.0);
    let cfg = DecisionCfg {
        card_trigger_percent: trigger,
        tight,
        low,
        card_skip_before_reset_minutes: skip,
    };
    let mut out = Vec::new();

    for c in payload["cases"].as_array().cloned().unwrap_or_default() {
        let mk = |t: i64, used: f64, total: f64, unlimited: bool| QuotaState {
            quota_type: t,
            total,
            used,
            is_unlimited: unlimited,
            reset_time_ms: None,
        };
        let five_used = c["five"].as_f64().unwrap_or(0.0);
        let week_used = c["week"].as_i64();
        // 每个用例可选带 resetInMinutes：距 5 小时额度刷新还有多少分钟
        let five_reset = c["resetInMinutes"]
            .as_f64()
            .map(|m| now + (m * 60_000.0) as i64);
        let mut quotas = vec![
            mk(1, five_used, 100.0, false),
            match week_used {
                None => mk(2, 0.0, 100.0, true),
                Some(w) => mk(2, w as f64, 100.0, false),
            },
        ];
        quotas[0].reset_time_ms = five_reset;
        let cards: Vec<UsableCard> = c["cards"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| {
                let d = v.as_i64()?;
                Some(UsableCard {
                    card_id: format!("C{d}"),
                    source_type: "mcode_signin".into(),
                    out_biz_no: format!("b{d}"),
                    expire_time: now + d * day,
                })
            })
            .collect();
        let credit = c["credit"].as_i64().map(|d| now + d * day);

        // 「积分数据是否完整」由用例给出（默认 true）——
        // parity 校验器用它比对三方在**数据不完整**时是否都拒绝写操作。
        let complete = c
            .get("creditDataComplete")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let d = decide(&quotas, &cards, credit, "平台积分", &cfg, now, complete);
        let five_level = quotas[0].level(tight, low);
        out.push(serde_json::json!({
            "n": c["n"].as_str().unwrap_or(""),
            "action": d.action.as_str(),
            "reason": d.reason,
            "level": five_level.as_str(),
        }));
    }
    println!("{}", serde_json::to_string(&out).unwrap_or_default());
}

fn main() {
    // 「无限重试」必须可中断：装上 SIGINT / SIGTERM 处理器，
    // 让 launchctl 能优雅停掉正卡在重试里的守护进程。
    api::install_signal_handlers();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        selftest();
        return;
    }
    // 自检：写一条唤醒请求（不直接投递）。
    // 投递由 minimax-bot 的 MiniApp 用 `agent.run` 完成 —— 守护进程在 Host 之外，
    // 没有对 bot 会话的归属权限，裸写队列项也不会被消费（2026-10-20 实测）。
    if let Some(i) = args.iter().position(|a| a == "--selftest-bot-revive") {
        let Some(session_id) = args.get(i + 1) else {
            eprintln!("用法: --selftest-bot-revive <sessionId> <botName>");
            std::process::exit(2);
        };
        let bot_name = args.get(i + 2).cloned().unwrap_or_else(|| "Bot".to_string());
        let bot = bot_guard::BotSession {
            bot_id: format!("bot-selftest-{bot_name}"),
            bot_name: bot_name.clone(),
            session_id: session_id.clone(),
            chat_id: format!("bot:bot-selftest-{bot_name}"),
        };
        match bot_guard::enqueue_revive_request(
            &bot,
            "【看门狗自检】请简短确认收到这条续跑消息。",
        ) {
            Ok(true) => println!("已写入唤醒请求：{session_id}"),
            Ok(false) => println!("已在待办里，跳过：{session_id}"),
            Err(e) => {
                eprintln!("写请求失败：{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let mut interval: f64 = 60.0;
    let mut dry_run = false;
    let mut cmd = "watch".to_string();
    let mut config_path: Option<std::path::PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" if i + 1 < args.len() => {
                interval = args[i + 1].parse().unwrap_or(60.0);
                i += 2;
            }
            "--config" if i + 1 < args.len() => {
                config_path = Some(std::path::PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--base" if i + 1 < args.len() => i += 2,
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "watch" | "once" | "status" | "forget" => {
                cmd = args[i].clone();
                i += 1;
            }
            _ => i += 1,
        }
    }

    if cmd == "forget" {
        state::clear_spent();
        println!("已清空本地去重记录。");
        return;
    }

    // 每轮重读：改配置无需 `launchctl unload/load`
    let load_cfg = || match &config_path {
        Some(p) => config::load(p),
        None => config::load_or_default(),
    };
    let (cfg, issues) = load_cfg();
    for s in &issues {
        log(&format!("⚠️  {s}"));
    }

    let client = match Client::new(BASE_CN) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("初始化失败：{e}");
            std::process::exit(1);
        }
    };
    let mut daemon = Daemon::new(client);

    if cmd == "once" {
        match daemon.tick(dry_run, &cfg) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("失败：{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    log(&format!(
        "启动后台监控，间隔 {interval}s（Ctrl+C 退出）；扣卡触发线 {:.0}%，额度分界 {:.0}%/{:.0}%",
        cfg.card_trigger_percent, cfg.quota_levels.tight, cfg.quota_levels.low
    ));
    log("扣卡策略：与 Mini App「蹬王之王」一致 —— 按先过期先用");
    if cfg.goal_pause.enabled {
        log(&format!(
            "Goal 自动暂停：开（到 {:.0}% 暂停，回落到 {:.0}% 恢复）",
            cfg.goal_pause.trigger_percent, cfg.goal_pause.resume_at_percent
        ));
    } else {
        log("Goal 自动暂停：关");
    }
    // 连续失败计数：单轮失败可能是网络抖动，连着失败就是「目标该停却没停」的
    // 危险状态，必须逐级喊出来，而不是每轮都被当成平常事刷过去。
    let mut fails: u32 = 0;
    loop {
        // 收到终止信号 ⇒ 立刻退出（可能在重试循环里被置位）
        if api::should_stop() {
            log("收到终止信号，守护进程退出");
            return;
        }
        // 轮内重读配置，页面改完下一轮就生效
        let (cfg, issues) = load_cfg();
        for s in &issues {
            log(&format!("⚠️  {s}"));
        }
        match daemon.tick(dry_run, &cfg) {
            Ok(_) => {
                if fails > 0 {
                    log(&format!("✓ 已恢复，本轮正常（此前连续失败 {fails} 轮）"));
                }
                fails = 0;
            }
            Err(e) if e.is_unauthorized() => {
                log(&format!(
                    "✗ 鉴权失败：{e}。请确认 MiniMax Code 桌面端已启动并处于登录状态。"
                ));
                std::process::exit(1);
            }
            Err(e) => {
                fails += 1;
                if fails == 5 {
                    log("🔴 连续失败 5 轮：Goal 自动暂停当前处于**失效**状态（该停的目标不会停）。");
                } else if fails % 30 == 0 {
                    log(&format!("🔴 已连续失败 {fails} 轮，Goal 自动暂停仍未恢复工作。"));
                } else {
                    log(&format!("⚠️  本轮失败（连续 {fails} 轮）：{e}，下一轮继续"));
                }
            }
        }
        // 抖动，避免与整点/整 5 小时重置撞在一起
        let jitter = (api::now_ms() % 1000) as f64 / 1000.0 * 4.0 - 2.0;
        std::thread::sleep(Duration::from_millis(((interval + jitter) * 1000.0) as u64));
    }
}
