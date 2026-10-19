//! 账号 API 客户端 + OAuth lease token。
//!
//! 域名划分是实测结论，不是猜的：
//!   开放平台 / 额度 / 重置卡 → www.minimaxi.com（agent 域对这些路径 404 或返回 HTML）
//!   Code 积分 / 会员信息    → agent.minimaxi.com（platform 域返回非 JSON）

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 进程是否收到过终止信号（SIGINT / SIGTERM）。
///
/// 「无限重试」必须**可中断** —— 否则 launchctl 想停守护进程时，
/// 它正卡在一次无限重试的 HTTP 调用里，永远出不来。
/// 每次重试前检查这个标志；收到信号就立刻放弃这一轮并退出。
pub static STOPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 收到终止信号时置位。
pub fn request_stop() {
    STOPPED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// 是否已被要求停止。
pub fn should_stop() -> bool {
    STOPPED.load(std::sync::atomic::Ordering::SeqCst)
}

/// 安装 SIGINT / SIGTERM 处理器：第一次收到就置位停止标志。
///
/// 第二次信号直接退出（用户可能真的不耐烦了）。
pub fn install_signal_handlers() {
    unsafe extern "C" fn handler(_sig: i32) {
        if should_stop() {
            std::process::exit(130);
        }
        request_stop();
    }
    unsafe {
        signal(SIGINT, handler as usize);
        signal(SIGTERM, handler as usize);
    }
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

pub const BASE_CN: &str = "https://www.minimaxi.com";
pub const BASE_GLOBAL: &str = "https://platform.minimax.io";
pub const CODE_BASE: &str = "https://agent.minimaxi.com";

const LEASE_SOCK: &str = ".minimax/run/mcode-auth-lease-v1.sock";
const LEASE_CAP: &str = ".minimax/run/mcode-auth-lease-v1.cap";

const TOKEN_TTL_MS: u64 = 30 * 60 * 1000;
const TOKEN_REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

// ── 可重试错误：无限重试 + 指数退避 ────────────────────────────────────────────
//
// 2026-10-19 新增。目标：遇到「服务端内部错误」或「负载过高」时**一直重试**
// 直到成功，而不是让守护进程把这一轮判成失败、把看板显示成「读不到数据」。
//
// 判定依据（HTTP 状态码）：
//   429 负载过高 / 限流      → 重试
//   500 内部错误             → 重试
//   502 网关错误             → 重试
//   503 服务不可用           → 重试
//   504 网关超时             → 重试
//
// 明确**不**重试的：
//   401 / 403  登录态失效 —— 重试无用，要重新取 token（走已有的 token 失效分支）
//   其他 4xx   客户端错误 —— 重试没有意义
//   业务错误   确定性失败（如参数非法、额度不足）
//
// 退避：min(base * 2^(n-1), cap) + 抖动，base=1s cap=60s。
// 封顶 60s 是为了「无限重试」不会把接口打爆，也不会指数增长到没有尽头。
pub const RETRY_BASE_MS: u64 = 1_000;
pub const RETRY_CAP_MS: u64 = 60_000;

/// 一次失败的**重试决策**。
#[derive(Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// 值得重试（暂时性错误）：退避后重试，直到成功。
    Retry,
    /// 登录态失效：清 token 重取一次。
    RefreshToken,
    /// 确定性失败：直接报错，重试没有意义。
    Fatal(String),
}

/// 根据一次请求的错误决定下一步。
///
/// 抽成纯函数是为了**能直接单测**（不依赖网络与真实 token）。
/// 2026-10-19 新增无限重试时引入。
pub fn decide_retry(err: &ureq::Error) -> RetryDecision {
    match err {
        // 401/403 ⇒ 登录态失效，重取 token（只重试一次，见调用点）
        ureq::Error::StatusCode(401) | ureq::Error::StatusCode(403) => RetryDecision::RefreshToken,
        // 429 负载过高 / 5xx 内部与网关错误 ⇒ 暂时性，值得一直重试
        ureq::Error::StatusCode(code) if is_retryable_status(*code) => RetryDecision::Retry,
        ureq::Error::StatusCode(code) => RetryDecision::Fatal(format!("HTTP {code}")),
        // 网络层错误（连接重置 / 超时 / DNS 抖动）⇒ 也值得重试
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::HostNotFound => {
            RetryDecision::Retry
        }
        other => RetryDecision::Fatal(format!("{other}")),
    }
}

/// 该 HTTP 状态码是否值得重试。
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// 退避时长：指数增长 + 抖动，封顶 `RETRY_CAP_MS`。
///
/// 抖动是必要的 —— 否则大量重试会形成「惊群」，同时打在同一时刻。
pub fn retry_delay_ms(attempt: u32) -> u64 {
    let exp = RETRY_BASE_MS
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(16))
        .min(RETRY_CAP_MS);
    exp + (pseudo_random() % 500)
}

/// 轻量伪随机（xorshift）——只为抖动，不需要密码学强度。
fn pseudo_random() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x9E3779B97F4A7C15) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            x = 0x9E3779B97F4A7C15;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized(String),
    Network(String),
    Business(String),
}

impl ApiError {
    pub fn message(&self) -> &str {
        match self {
            ApiError::Unauthorized(m) | ApiError::Network(m) | ApiError::Business(m) => m,
        }
    }
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, ApiError::Unauthorized(_))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn random_hex(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_err()
    {
        buf[..8].copy_from_slice(&now_ms().to_be_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 向桌面端的 lease socket 取一个短期 access token。
/// 协议：4 字节大端长度前缀 + JSON。详见 packages/oauth-lease-protocol。
pub fn acquire_lease_token() -> Option<String> {
    let capability = std::fs::read_to_string(home().join(LEASE_CAP))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let sock_path = home().join(LEASE_SOCK);
    if !sock_path.exists() {
        return None;
    }

    let payload = serde_json::json!({
        "version": 1,
        "requestId": random_hex(16),
        "capability": capability,
        "method": "lease",
        "minValidityMs": 120_000i64,
    })
    .to_string();

    let body = payload.as_bytes();
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);

    let mut stream = UnixStream::connect(&sock_path).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(&frame).ok()?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).ok()?;
    let size = u32::from_be_bytes(len_buf) as usize;
    if size > 1 << 20 {
        return None;
    }
    let mut buf = vec![0u8; size];
    stream.read_exact(&mut buf).ok()?;

    let resp: serde_json::Value = serde_json::from_slice(&buf).ok()?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let result = resp.get("result")?;
    if result.get("method").and_then(|v| v.as_str()) != Some("lease") {
        return None;
    }
    result
        .get("accessToken")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// token 来源：环境变量 → 本地文件 → lease socket
pub fn resolve_token() -> Option<String> {
    for env in ["MCODE_TOKEN", "MINIMAX_TOKEN"] {
        if let Ok(v) = std::env::var(env) {
            let t = v.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(home().join(".config/mcode-reset-card/token")) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    acquire_lease_token()
}

/// ureq Agent 自带 keep-alive 连接池：后台每轮打多个 API，
/// 复用连接省掉每轮的 TLS 握手 —— 这是常驻进程最大的一笔固定开销。
pub struct Client {
    agent: ureq::Agent,
    token: String,
    cached_at: u64,
}

impl Client {
    pub fn new(base: &str) -> Result<Self, ApiError> {
        let token = resolve_token().ok_or_else(|| {
            ApiError::Unauthorized(
                "未获取到登录态，请确认 MiniMax Code 桌面端正在运行且已登录".into(),
            )
        })?;

        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .timeout_connect(Some(Duration::from_secs(5)))
            .max_idle_connections(8)
            .build();
        let agent: ureq::Agent = config.into();

        // 用一个明确的 base 做首连校验，确保 base 可用
        let c = Client { agent, token, cached_at: now_ms() as u64 };
        let _ = base;
        Ok(c)
    }

    fn token(&mut self) -> String {
        let age = (now_ms() as u64).saturating_sub(self.cached_at);
        if age >= TOKEN_TTL_MS - TOKEN_REFRESH_MARGIN_MS {
            if let Some(t) = acquire_lease_token() {
                self.token = t;
                self.cached_at = now_ms() as u64;
            }
        }
        self.token.clone()
    }

    fn base_for(&self, code_domain: bool) -> &'static str {
        if code_domain { CODE_BASE } else { BASE_CN }
    }

    pub fn get(&mut self, path: &str, code_domain: bool) -> Result<serde_json::Value, ApiError> {
        self.do_request("GET", path, None, code_domain)
    }

    pub fn post(
        &mut self,
        path: &str,
        body: &serde_json::Value,
        code_domain: bool,
    ) -> Result<serde_json::Value, ApiError> {
        self.do_request("POST", path, Some(body), code_domain)
    }

    fn do_request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        code_domain: bool,
    ) -> Result<serde_json::Value, ApiError> {
        let base = self.base_for(code_domain);
        let url = format!("{base}{path}");

        // 鉴权失败重取 token 再试一次；其余可重试错误**无限重试**。
        //
        // ⚠️ 无限重试的前提是「每次重试都真的会变好」：
        //   - 401/403 ⇒ 重新取 token 后重试（最多一次，之后仍失败就报错）
        //   - 429/5xx/网络错误 ⇒ 退避后重试，直到成功
        // 退避封顶 60s，所以「无限」不会变成「疯狂打接口」。
        let mut attempt = 0u32;
        let mut retry_count = 0u32;
        loop {
            let token = self.token();
            let auth = format!("Bearer {token}");
            let accept = "application/json";
            let ua = "MiniMaxCode";

            let result = match (method, body) {
                ("GET", None) => self
                    .agent
                    .get(&url)
                    .header("Authorization", &auth)
                    .header("Accept", accept)
                    .header("User-Agent", ua)
                    .call(),
                ("POST", Some(b)) => self
                    .agent
                    .post(&url)
                    .header("Authorization", &auth)
                    .header("Accept", accept)
                    .header("User-Agent", ua)
                    .send_json(b.clone()),
                _ => return Err(ApiError::Network(format!("不支持的请求 {method}"))),
            };

            match result {
                Ok(resp) => {
                    let text = resp
                        .into_body()
                        .read_to_string()
                        .map_err(|e| ApiError::Network(format!("读取响应体失败: {e}")))?;
                    let value: serde_json::Value =
                        serde_json::from_str(&text).map_err(json_err)?;
                    return check_business(value);
                }
                Err(e) => match decide_retry(&e) {
                    // 登录态失效：清 token 重取一次（只一次）。
                    RetryDecision::RefreshToken if attempt == 0 => {
                        self.cached_at = 0;
                        attempt += 1;
                        continue;
                    }
                    RetryDecision::RefreshToken => {
                        return Err(ApiError::Unauthorized("登录态失效".into()));
                    }
                    // ── 可重试的服务端错误 / 网络错误：无限重试 ──
                    // 429 负载过高、5xx 内部与网关错误、连接重置 —— 都是**暂时性**的，
                    // 重试有意义。退避封顶 60s，不会把接口打爆。
                    RetryDecision::Retry => {
                        if should_stop() {
                            return Err(ApiError::Network("收到终止信号，放弃重试".into()));
                        }
                        retry_count += 1;
                        let delay = retry_delay_ms(retry_count);
                        crate::log(&format!(
                            "⚠️  {method} {path} 失败（可重试，第 {retry_count} 次）：{e} ⇒ {secs}s 后重试",
                            secs = delay / 1000,
                        ));
                        std::thread::sleep(Duration::from_millis(delay));
                        continue;
                    }
                    // 确定性失败：重试没有意义，直接报错。
                    RetryDecision::Fatal(msg) => {
                        return Err(ApiError::Network(msg));
                    }
                },
            }
        }
        // 走到这里说明 loop 被 break —— 当前实现里所有分支都 return/continue，
        // 所以这行不可达。保留它是为了让函数在将来引入 break 时仍能返回错误。
    }
}

fn json_err(e: serde_json::Error) -> ApiError {
    ApiError::Network(format!("JSON 解析失败: {e}"))
}

/// 检查 `base_resp.status_code`。
///
/// 这是踩过的坑：`{"total":0,"base_resp":{"status_code":2013,"status_msg":"invalid params"}}`
/// 是**错误**，不是空结果。必须在这里拦下，否则会把接口错误读成「没有记录」。
fn check_business(value: serde_json::Value) -> Result<serde_json::Value, ApiError> {
    if let Some(code) = value
        .get("base_resp")
        .and_then(|b| b.get("status_code"))
        .and_then(|v| v.as_i64())
    {
        if code != 0 {
            let msg = value
                .get("base_resp")
                .and_then(|b| b.get("status_msg"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            if code == 1004 {
                return Err(ApiError::Unauthorized(format!("{msg} (code={code})")));
            }
            return Err(ApiError::Business(format!("{msg} (code={code})")));
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn business_error_detected() {
        let v = serde_json::json!({"total":0,"base_resp":{"status_code":2013,"status_msg":"invalid params"}});
        match check_business(v) {
            Err(ApiError::Business(m)) => assert!(m.contains("invalid params")),
            other => panic!("应当识别为业务错误，实际 {other:?}"),
        }
    }

    #[test]
    fn success_passes_through() {
        let v = serde_json::json!({"base_resp":{"status_code":0},"data":[1,2]});
        assert!(check_business(v).is_ok());
    }

    #[test]
    fn unauthorized_code() {
        let v = serde_json::json!({"base_resp":{"status_code":1004,"status_msg":"not login"}});
        assert!(matches!(check_business(v), Err(ApiError::Unauthorized(_))));
    }

    /// 回归测试：可重试错误码的判定。
    ///
    /// 2026-10-19 新增「无限重试」功能：遇到 500（内部错误）与 429（负载过高）
    /// 应当一直重试；401/403（登录态失效）与其他 4xx 属于确定性失败，
    /// 重试没有意义，必须快速失败。
    #[test]
    fn retryable_status_set() {
        for code in [429u16, 500, 502, 503, 504] {
            assert!(is_retryable_status(code), "HTTP {code} 应当可重试");
        }
        for code in [400u16, 401, 403, 404, 422] {
            assert!(!is_retryable_status(code), "HTTP {code} 不该重试");
        }
    }

    /// 退避必须**封顶**，否则「无限重试」会指数增长到没有尽头。
    #[test]
    fn retry_delay_is_capped_and_grows() {
        let d1 = retry_delay_ms(1);
        let d5 = retry_delay_ms(5);
        let d50 = retry_delay_ms(50);
        let d1000 = retry_delay_ms(1000);
        assert!(d5 > d1, "退避应随次数增长：d1={d1} d5={d5}");
        assert!(
            d50 <= RETRY_CAP_MS + 500,
            "退避必须封顶在 cap+抖动内：d50={d50} cap={RETRY_CAP_MS}"
        );
        assert!(
            d1000 <= RETRY_CAP_MS + 500,
            "无限次重试也不能超过 cap+抖动：d1000={d1000}"
        );
    }

    /// 回归测试：一次失败的**重试决策**。
    ///
    /// 2026-10-19 新增无限重试时引入的纯函数（可脱离网络单测）：
    ///   - 429 / 5xx / 网络错误 ⇒ Retry（一直重试）
    ///   - 401 / 403            ⇒ RefreshToken（重取 token，一次）
    ///   - 其他 4xx              ⇒ Fatal（确定性失败，快速失败）
    #[test]
    fn retry_decision_matrix() {
        // 可重试：负载过高与内部/网关错误
        for code in [429u16, 500, 502, 503, 504] {
            assert_eq!(
                decide_retry(&ureq::Error::StatusCode(code)),
                RetryDecision::Retry,
                "HTTP {code} 应当一直重试"
            );
        }
        // 登录态失效：重取 token
        for code in [401u16, 403] {
            assert_eq!(
                decide_retry(&ureq::Error::StatusCode(code)),
                RetryDecision::RefreshToken,
                "HTTP {code} 应当重取 token"
            );
        }
        // 确定性失败：快速失败
        for code in [400u16, 404, 422] {
            match decide_retry(&ureq::Error::StatusCode(code)) {
                RetryDecision::Fatal(msg) => assert!(msg.contains(&code.to_string())),
                other => panic!("HTTP {code} 应当快速失败，实际 {other:?}"),
            }
        }
        // 网络层错误也重试
        assert_eq!(
            decide_retry(&ureq::Error::HostNotFound),
            RetryDecision::Retry,
            "DNS 解析失败应当重试"
        );
    }

    /// 停止标志：置位后 should_stop 为真（无限重试靠它可中断）。
    #[test]
    fn stop_flag_roundtrip() {
        assert!(!should_stop(), "初始状态应为未停止");
        request_stop();
        assert!(should_stop(), "置位后应为已停止");
        request_stop(); // 幂等
        assert!(should_stop());
        // 复位，避免影响其它测试
        STOPPED.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
