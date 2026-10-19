//! 共享配置：与 Mini App「蹬王之王」读**同一个文件**。
//!
//! 这是三份实现不漂移的前提：Node / Rust / Python 都从这里取阈值与分界点，
//! 谁都不许再把 99 写死在自己代码里。
//!
//! 路径 `~/.config/mcode-reset-card/config.json` 与 Mini App 侧一致
//! （Rust `state.rs` 的 bg-state.json 也在同一目录）。
//!
//! 读失败或字段非法一律**回退默认并记 issue**，绝不静默纠正 ——
//! 静默纠正会让人以为自己设的生效了。

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::decision::{
    DEFAULT_CARD_SKIP_BEFORE_RESET_MINUTES, DEFAULT_QUOTA_LOW_PERCENT, DEFAULT_QUOTA_TIGHT_PERCENT,
    DEFAULT_QUOTA_TRIGGER_PERCENT,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaLevels {
    #[serde(default = "d_tight")]
    pub tight: f64,
    #[serde(default = "d_low")]
    pub low: f64,
}

fn d_tight() -> f64 {
    DEFAULT_QUOTA_TIGHT_PERCENT
}
fn d_low() -> f64 {
    DEFAULT_QUOTA_LOW_PERCENT
}
fn d_card() -> f64 {
    DEFAULT_QUOTA_TRIGGER_PERCENT
}

impl Default for QuotaLevels {
    fn default() -> Self {
        QuotaLevels { tight: DEFAULT_QUOTA_TIGHT_PERCENT, low: DEFAULT_QUOTA_LOW_PERCENT }
    }
}

/// `rename_all = "camelCase"` **不是可有可无的风格问题**：
/// Node / Python 写出的配置键是 `goalPause.triggerPercent` 这类驼峰，
/// 而 Rust 结构体字段是 snake_case。没有这行，serde 会认为**所有字段都不存在**，
/// 然后逐个落到 `#[serde(default)]` —— 于是 `enabled` 永远读到 `false`，
/// 且**一行错都不报**。这个坑真实发生过：功能看起来写完了，却永远打不开。
///
/// `deny_unknown_fields` 是配套的安全网：宁可解析失败并报出来，
/// 也不要「静默降级成默认值」让人以为自己设的值生效了。
/// Bot 守护配置（守护 MiniMax Bot 团队）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotGuardCfg {
    /// 总开关。默认关 —— 需要用户明确开启。
    #[serde(default)]
    pub enabled: bool,
    /// 超过多久没有任何活动才算「闲下来」（毫秒）。
    #[serde(default = "d_bot_idle")]
    pub idle_ms: i64,
    /// 退避基数（毫秒）。
    #[serde(default = "d_bot_backoff_base")]
    pub backoff_base_ms: i64,
    /// 退避上限（毫秒）。
    #[serde(default = "d_bot_backoff_cap")]
    pub backoff_cap_ms: i64,
}

fn d_bot_idle() -> i64 {
    90_000
}
fn d_bot_backoff_base() -> i64 {
    5_000
}
fn d_bot_backoff_cap() -> i64 {
    60_000
}

impl Default for BotGuardCfg {
    fn default() -> Self {
        BotGuardCfg {
            enabled: false,
            idle_ms: d_bot_idle(),
            backoff_base_ms: d_bot_backoff_base(),
            backoff_cap_ms: d_bot_backoff_cap(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalPauseCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_card")]
    pub trigger_percent: f64,
    #[serde(default)]
    pub resume_at_percent: f64,
}

impl Default for GoalPauseCfg {
    fn default() -> Self {
        GoalPauseCfg { enabled: false, trigger_percent: DEFAULT_QUOTA_TRIGGER_PERCENT, resume_at_percent: 0.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default = "one")]
    pub schema_version: u32,
    #[serde(default = "d_card")]
    pub card_trigger_percent: f64,
    /// 距 5 小时额度刷新不足这么多分钟时，不消耗重置卡。0 = 关闭该保护。
    #[serde(default = "d_skip")]
    pub card_skip_before_reset_minutes: f64,
    /// 「目标恢复值守」cron 的 id。守护进程按需武装/解除武装它
    /// —— 没有它，恢复就没人执行；一直开着，它又每轮都在烧 token。
    #[serde(default = "d_cron")]
    pub resume_cron_id: String,
    #[serde(default)]
    pub quota_levels: QuotaLevels,
    #[serde(default)]
    pub goal_pause: GoalPauseCfg,
    /// 守护 MiniMax Bot 团队：挂掉了自动重新启用。
    #[serde(default)]
    pub bot_guard: BotGuardCfg,
}

fn one() -> u32 {
    1
}

fn d_skip() -> f64 {
    DEFAULT_CARD_SKIP_BEFORE_RESET_MINUTES
}

fn d_cron() -> String {
    String::new()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema_version: 1,
            card_trigger_percent: DEFAULT_QUOTA_TRIGGER_PERCENT,
            card_skip_before_reset_minutes: DEFAULT_CARD_SKIP_BEFORE_RESET_MINUTES,
            resume_cron_id: String::new(),
            quota_levels: QuotaLevels::default(),
            goal_pause: GoalPauseCfg::default(),
            bot_guard: BotGuardCfg::default(),
        }
    }
}

impl Config {
    /// 判定参数视图，供 `decide()` 使用。
    pub fn decision_cfg(&self) -> crate::decision::DecisionCfg {
        crate::decision::DecisionCfg {
            card_trigger_percent: self.card_trigger_percent,
            tight: self.quota_levels.tight,
            low: self.quota_levels.low,
            card_skip_before_reset_minutes: self.card_skip_before_reset_minutes,
        }
    }
}

fn clamp(v: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if !v.is_finite() {
        return fallback;
    }
    v.max(lo).min(hi)
}

/// 逐字段校验并夹紧，返回 (配置, 问题列表)。
pub fn normalize(mut raw: Config) -> (Config, Vec<String>) {
    let mut issues = Vec::new();
    let d = Config::default();

    let card = clamp(raw.card_trigger_percent, 1.0, 100.0, d.card_trigger_percent);
    if card != raw.card_trigger_percent {
        issues.push(format!(
            "card_trigger_percent={} 超出 1..100，已夹到 {}",
            raw.card_trigger_percent, card
        ));
    }
    raw.card_trigger_percent = card;

    // 跳过阈值：0 表示关闭该保护；负数没有意义，按 0 夹。
    let skip = clamp(raw.card_skip_before_reset_minutes, 0.0, 24.0 * 60.0, d.card_skip_before_reset_minutes);
    if skip != raw.card_skip_before_reset_minutes {
        issues.push(format!(
            "card_skip_before_reset_minutes={} 超出 0..1440，已夹到 {skip}",
            raw.card_skip_before_reset_minutes
        ));
    }
    raw.card_skip_before_reset_minutes = skip;

    let mut tight = clamp(raw.quota_levels.tight, 1.0, 100.0, d.quota_levels.tight);
    let mut low = clamp(raw.quota_levels.low, 1.0, 100.0, d.quota_levels.low);
    // 分界点必须 0 < tight < low <= 100；非法组合整体回退默认，而不是悄悄调序。
    if !(tight < low) {
        issues.push(format!(
            "quota_levels 需满足 tight < low（收到 tight={} low={}），已整体回退默认",
            tight, low
        ));
        tight = d.quota_levels.tight;
        low = d.quota_levels.low;
    } else if tight != raw.quota_levels.tight || low != raw.quota_levels.low {
        issues.push("quota_levels 越界，已夹紧".to_string());
    }
    raw.quota_levels.tight = tight;
    raw.quota_levels.low = low;

    let t = clamp(raw.goal_pause.trigger_percent, 1.0, 100.0, d.goal_pause.trigger_percent);
    if t != raw.goal_pause.trigger_percent {
        issues.push(format!(
            "goal_pause.trigger_percent={} 超出 1..100，已夹到 {}",
            raw.goal_pause.trigger_percent, t
        ));
    }
    raw.goal_pause.trigger_percent = t;

    let r = clamp(raw.goal_pause.resume_at_percent, 0.0, 100.0, d.goal_pause.resume_at_percent);
    if r != raw.goal_pause.resume_at_percent {
        issues.push(format!(
            "goal_pause.resume_at_percent={} 超出 0..100，已夹到 {}",
            raw.goal_pause.resume_at_percent, r
        ));
    }
    raw.goal_pause.resume_at_percent = r;

    (raw, issues)
}

pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/mcode-reset-card/config.json"))
}

/// 读配置。文件不存在是正常情况（首次运行），返回默认值且不记 issue。
pub fn load(path: &PathBuf) -> (Config, Vec<String>) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), Vec::new()),
        Err(e) => return (Config::default(), vec![format!("配置读取失败（{}），正在用默认值", e)]),
    };
    match serde_json::from_str::<Config>(&text) {
        Ok(c) => normalize(c),
        Err(e) => (Config::default(), vec![format!("配置解析失败（{}），正在用默认值", e)]),
    }
}

/// 每轮重读，实现「改配置无需重启守护进程」。
pub fn load_or_default() -> (Config, Vec<String>) {
    match default_path() {
        Some(p) => load(&p),
        None => (
            Config::default(),
            vec!["找不到 $HOME，配置不可读，正在用默认值".to_string()],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let c = Config::default();
        assert_eq!(c.card_trigger_percent, 99.0);
        assert_eq!(c.quota_levels.tight, 80.0);
        assert_eq!(c.quota_levels.low, 95.0);
        assert!(!c.goal_pause.enabled, "Goal 自动暂停默认关");
        assert_eq!(c.goal_pause.resume_at_percent, 0.0);
    }

    #[test]
    fn out_of_range_is_clamped_and_reported() {
        let (c, issues) = normalize(Config {
            card_trigger_percent: 150.0,
            ..Default::default()
        });
        assert_eq!(c.card_trigger_percent, 100.0);
        assert!(!issues.is_empty(), "越界必须报出来，不能静默");
    }

    #[test]
    fn inverted_bounds_fall_back_whole() {
        let (c, issues) = normalize(Config {
            quota_levels: QuotaLevels { tight: 96.0, low: 95.0 },
            ..Default::default()
        });
        assert_eq!(c.quota_levels.tight, 80.0, "非法组合应整体回退默认");
        assert_eq!(c.quota_levels.low, 95.0);
        assert!(issues.iter().any(|s| s.contains("tight < low")));
    }

    #[test]
    fn camel_case_keys_are_actually_read() {
        // 回归测试：Node / Python 写出的就是这套驼峰键。
        // 一旦有人删掉 rename_all，下面这条会安静地读成 enabled=false 而测试仍可能通过，
        // 所以这里直接断言「解析成功且 enabled 为真」，不给它蒙混过关的机会。
        let (c, issues) = load(&PathBuf::from(
            std::env::temp_dir().join("cfg-camel-probe.json"),
        ));
        // 文件不存在 → 默认值，这条只用来确认默认路径本身没坏
        assert!(!c.goal_pause.enabled);
        assert!(issues.is_empty());

        let camel = serde_json::json!({
            "schemaVersion": 1,
            "cardTriggerPercent": 99,
            "quotaLevels": { "tight": 80, "low": 95 },
            "goalPause": { "enabled": true, "triggerPercent": 50, "resumeAtPercent": 0 }
        });
        let parsed: Config = serde_json::from_value(camel.clone())
            .expect("驼峰键必须能被解析（缺 rename_all 时会在这里炸）");
        assert!(parsed.goal_pause.enabled, "驼峰 goalPause.enabled 必须真的生效");
        assert_eq!(parsed.goal_pause.trigger_percent, 50.0);
        assert_eq!(parsed.card_trigger_percent, 99.0);

        // 反面：snake_case 现在应当被明确拒绝，而不是静默降级
        let snake = serde_json::json!({
            "schema_version": 1,
            "goal_pause": { "enabled": true, "trigger_percent": 50 }
        });
        assert!(
            serde_json::from_value::<Config>(snake).is_err(),
            "snake_case 必须报错（deny_unknown_fields），绝不能静默降级"
        );
    }

    #[test]
    fn valid_bounds_are_kept() {
        let (c, issues) = normalize(Config {
            quota_levels: QuotaLevels { tight: 70.0, low: 90.0 },
            ..Default::default()
        });
        assert_eq!(c.quota_levels.tight, 70.0);
        assert_eq!(c.quota_levels.low, 90.0);
        assert!(issues.is_empty());
    }
}
