//! 唤醒链路的**自检**：宿主重启后，蹬王之王自己的 MiniApp 可能没被拉起来，
//! 于是「判定」正常、「投递」空转 —— 守护进程必须把这件事**说出来**。
//!
//! # 背景：为什么这里不能自动修复（2026-10-20 穷尽验证的结果）
//!
//! 唤醒的执行方是**蹬王之王自己的 MiniApp**（它有 `agentAccess:["session.run"]`，
//! 能用 `agent.run` 起回合，再用 mavis 工具的 `session send` 唤醒目标 bot）。
//! 但 MiniApp 是 **on-demand** 的，宿主**不会自动拉起它** —— 实测宿主 09:39 重启后，
//! 蹬王之王的运行时直到 13:18 才起来（我手动 open 的）。
//!
//! 而「从宿主外拉起 MiniApp」这条路，**逐条实测全部堵死**：
//!
//! | 通道 | 结果 | 原因 |
//! | --- | --- | --- |
//! | 往 `local_runtime_queue_items` 写消息 | ❌ | 队列轮询器只在会话有活跃 dispatch 时取项；**连正在跑的会话都不取**（实测：连投 idle 与 started 两种会话，3 分钟内都是 `queued` + `claim_id` 为空） |
//! | MiniApp 的 HTTP（55932） | ❌ | `host-runner.js` 的 `installMiniAppLoopbackGuard` 要 `Host: 127.0.0.1:<port>` + `x-mavis-miniapp-token`；token **只存在于宿主进程**，不传给 MiniApp 子进程 |
//! | 宿主 HTTP（58925） | ❌ | 同上（per-run 凭据），且宿主只有 3 个路由，没有起回合/拉 MiniApp 的接口 |
//! | `minimax-cn://chat?message=` | ❌ | `chat` handler 由渲染进程注册；实测 deeplink 触发不了回合 |
//! | `minimax-cn://navigate?url=` | ❌ | 只导航渲染进程页面，不会启动 MiniApp 运行时 |
//! | 官方 minimax-bot 插件的 MCP | ❌ | 它自己也是 MiniApp ⇒ 同样 on-demand；且官方插件不可改版 |
//!
//! ⇒ **从宿主外无法自动拉起 MiniApp**。这不是本插件的设计取舍，是宿主的边界。
//!
//! # 那能做什么
//!
//! 1. **检测**：运行时没起 + 有待唤醒请求 ⇒ 这是「判定正常但投递空转」的状态。
//! 2. **说清楚**：把这个状态写进日志与面板，让用户知道「需要打开一次蹬王之王面板」。
//!    ⚠️ 绝不静默 —— 静默的话，守护进程看起来一切正常，实际一个 bot 都没被叫醒。
//! 3. **不打扰**：只在「确实有 pending 请求」时才提示；没有 pending 就不出声。
//!
//! # 唯一确认可行的自举路径（供未来参考）
//!
//! 宿主在准备**任何一个回合**时，只要消息里出现 `@<pluginId>`，就会 `ensureStarted`
//! 把那个 MiniApp 拉起来（`turn-preparation.ts` 的 `prepareReferencedMiniApps`），
//! 前提是该插件**声明了 MCP 端点**（`referenced.filter(c => c.mcpEndpoints.length > 0)`）。
//! 实测有效：2026-10-20 14:37 我往一个活跃会话发了一条 `@expiry-guardian 看门狗探针`，
//! 宿主随即拉起了蹬王之王的运行时。
//! ⇒ 只要**宿主内有人发一条带 `@expiry-guardian` 的消息**，自举就成立。
//! 这需要宿主内的 `session send`（进程内 adapter），宿主外拿不到 —— 故本模块只做检测与提示。

use std::path::{Path, PathBuf};

/// 插件包名（= 运行时 package 目录里 `package.json` 的 `name` 字段）。
///
/// 用它区分「蹬王之王的运行时」与「别的 MiniApp 的运行时」——
/// 实测同时存在 3 个 `mavis-miniapp-*` 目录，其中只有 1 个有活进程。
pub const PLUGIN_NAME: &str = "expiry-guardian";

/// 宿主为 MiniApp 运行时建的临时目录名标记。
const MINIAPP_PACKAGE_MARKER: &str = "mavis-miniapp-";

/// 自检结果。
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum SelfCheck {
    /// **没有待唤醒请求** —— 无论运行时在不在，都无需打扰用户。
    Idle,
    /// **有请求，运行时也在跑** ⇒ 投递链路正常。
    Delivering { pending: usize },
    /// **有请求投不出去**：运行时没起（多半是宿主重启后没被拉起）。
    ///
    /// 这是唯一需要用户知道的状态：判定在工作，但唤醒没在工作。
    NeedsAttention { pending: usize },
    /// 自检本身出错（不影响其它功能）。
    Failed(String),
}

/// 蹬王之王的 MiniApp 运行时是否在跑。
///
/// 判据：**有一个活着的进程，它的工作目录正是本插件的运行时包目录**。
///
/// ⚠️ 为什么不用「端口在监听」：
/// - 端口号是宿主的默认分配值，宿主改规则就会误判；
/// - 而且端口属于**宿主进程**（MiniApp 运行时是它 fork 出来的子进程，进程名继承为 `MiniMax`），
///   端口活着不等于**是蹬王之王**在跑（别的 MiniApp 也占端口）。
///
/// ⚠️ 为什么不用「目录存在」：
/// 实测 3 个 `mavis-miniapp-*` 目录同时存在，但只有 1 个对应活进程
/// ⇒ 停掉的运行时目录不会被清掉，光看目录会以为「在跑」。
///
/// 组合起来（活进程 + 目录归属）才是可靠判据。
pub fn runtime_alive() -> Result<bool, String> {
    // `lsof -a -d cwd -F` 列出**所有活进程**的工作目录。
    // 只列活进程 ⇒ 「目录还在但进程没了」这种假阳性被排除。
    //
    // ⚠️ 用 `-F`（字段模式）而不是默认的定宽表格（2026-10-20 二轮独立审计 F5）：
    // 默认模式下路径是最后一列，**路径含空格就会被切碎**
    // ⇒ `ends_with("/package")` 恒假 ⇒ 每轮都误判「运行时没起」
    // ⇒ 每轮都催用户去开一个已经开着的面板。
    // `-F` 模式下路径是 `n` 字段的**整行剩余部分**，空格保留 ✓。
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-a", "-d", "cwd", "-F"])
        .output()
        .map_err(|e| format!("无法执行 lsof（判据失效，不当成「没起」）：{e}"))?;
    // ⚠️ **stderr 非空 ⇒ 判据失效，不是「没起」**（2026-10-20 独立审计 F3）。
    // lsof 出错时：退出码=1、stdout 空、stderr 有 `can't get UID for …`
    // ⇒ 与「没有匹配」的退出码一样，**光看退出码区分不出来**。
    // 而「判据失效」与「运行时真的没起」必须分开：
    // 前者不该让用户去开一个已经开着的面板。
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        return Err(format!(
            "lsof 报错（判据失效，不当成「没起」）：{}",
            stderr.trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // 字段模式：一个进程一段，`f<fd>` 标明接下来的 `n<name>` 属于哪个 fd。
    let mut fd_is_cwd = false;
    for line in text.lines() {
        if let Some(fd) = line.strip_prefix('f') {
            fd_is_cwd = fd == "cwd";
            continue;
        }
        let Some(name) = line.strip_prefix('n') else { continue };
        if !fd_is_cwd {
            continue;
        }
        if name.contains(MINIAPP_PACKAGE_MARKER)
            && name.ends_with("/package")
            && package_is_ours(name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 这个运行时包目录是不是蹬王之王的。
///
/// ⚠️ **两个锚点都要认**（2026-10-20 独立审计 F2）：
/// 实测三个运行时目录里，`package.json` 的 `name` 字段**不总是有**：
///
/// | 目录 | `package.json` name | `.minimax-plugin/plugin.json` name | 只认 package.json？ |
/// | --- | --- | --- | --- |
/// | `mavis-miniapp-J3CVd3`（活进程） | expiry-guardian | expiry-guardian | ✅ |
/// | `mavis-miniapp-MxVUvr` | minimax-bot | minimax-bot | ✅（正确排除） |
/// | `mavis-miniapp-ZIU9nz` | **（无）** | expiry-guardian | ❌ **会漏判** |
///
/// ⇒ 只认 `package.json` 会把蹬王之王误判成「没在跑」
/// ⇒ 每轮都让用户去开一个**已经开着**的面板。
/// ⇒ 两个锚点**任意一个**匹配即算 ours。
fn package_is_ours(package_dir: &str) -> bool {
    let dir = PathBuf::from(package_dir);
    if name_of(&dir.join("package.json")).as_deref() == Some(PLUGIN_NAME) {
        return true;
    }
    name_of(&dir.join(".minimax-plugin").join("plugin.json")).as_deref() == Some(PLUGIN_NAME)
}

/// 读一个 JSON 文件里的 `name` 字段。
fn name_of(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
}

/// 跑一轮自检（真实探测）。
pub fn check(pending: usize) -> SelfCheck {
    match runtime_alive() {
        Ok(alive) => check_with(pending, alive),
        Err(e) => SelfCheck::Failed(e),
    }
}

/// 跑一轮自检（运行时状态已知时的纯逻辑版本 —— 测试用）。
pub fn check_with(pending: usize, runtime_alive: bool) -> SelfCheck {
    if pending == 0 {
        return SelfCheck::Idle;
    }
    if runtime_alive {
        SelfCheck::Delivering { pending }
    } else {
        SelfCheck::NeedsAttention { pending }
    }
}

/// 「投递空转」的观测结果。
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum StuckEvent {
    /// 没什么可说。
    None,
    /// 刚刚开始出现「积压条数不再变化」。
    Started,
    /// 积压条数连续没变超过阈值 ⇒ 判定为空转。
    Stuck { pending: usize, rounds: u32 },
    /// 积压条数变了 ⇒ 空转解除。
    Recovered,
}

/// 「投递空转」观测器。
///
/// 场景：运行时**在跑**，但待唤醒请求的积压条数一直不减少
/// ⇒ 请求没人消费（唤醒循环死了 / 回合一直失败 / 消费方卡住）。
/// 守护进程在跑、看起来一切正常，实际一个 bot 都没被叫醒 —— 这正是要避免的静默失败。
///
/// 判据用「条数**没变**」而不是「条数**没归零**」：
/// - 正常投递时，成功一个 ack 一个 ⇒ 条数是**逐步下降**的，过程里会变；
/// - 完全没人消费时，条数**一直不动**。
#[derive(Debug, Default)]
pub struct StuckWatch {
    last_pending: usize,
    streak: u32,
    reported: bool,
}

impl StuckWatch {
    /// 构造（默认没有任何观测）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 观察一轮的 pending 条数，返回这一轮该不该说话。
    pub fn observe(&mut self, pending: usize, threshold: u32) -> StuckEvent {
        if pending == 0 {
            let was_reported = self.reported;
            self.last_pending = 0;
            self.streak = 0;
            self.reported = false;
            return if was_reported {
                StuckEvent::Recovered
            } else {
                StuckEvent::None
            };
        }
        if pending != self.last_pending {
            let was_reported = self.reported;
            // ⚠️ **只有「变少」才算恢复**（2026-10-20 独立审计 F4）。
            // 「条数变了」包含「变多」——积压**增加**（又挂了新 bot）时
            // 说「已恢复消费」是**假话**（消费端其实一直死着）；
            // 「变少」也可能是某条请求超龄被清掉，消费端同样没在动。
            // ⇒ 真正的恢复信号只有一个：**积压在减少**。
            let recovered = was_reported && pending < self.last_pending;
            self.last_pending = pending;
            self.streak = 1;
            self.reported = false;
            return if recovered {
                StuckEvent::Recovered
            } else {
                StuckEvent::Started
            };
        }
        self.streak += 1;
        if self.streak >= threshold && !self.reported {
            self.reported = true;
            return StuckEvent::Stuck {
                pending,
                rounds: self.streak,
            };
        }
        StuckEvent::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_when_no_pending() {
        // 没有待唤醒请求 ⇒ 绝不出声（不打扰），无论运行时在不在。
        assert_eq!(check_with(0, true), SelfCheck::Idle);
        assert_eq!(check_with(0, false), SelfCheck::Idle);
    }

    #[test]
    fn delivering_when_runtime_alive() {
        assert_eq!(check_with(3, true), SelfCheck::Delivering { pending: 3 });
    }

    #[test]
    fn needs_attention_when_runtime_down() {
        assert_eq!(check_with(3, false), SelfCheck::NeedsAttention { pending: 3 });
    }

    #[test]
    fn package_name_is_read_from_package_json() {
        let dir = std::env::temp_dir().join(format!(
            "expiry-guardian-selftest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pkg = dir.join("package.json");
        std::fs::write(&pkg, r#"{"name":"expiry-guardian","version":"1.6.0"}"#).unwrap();
        assert_eq!(name_of(&pkg).as_deref(), Some(PLUGIN_NAME));
        assert!(package_is_ours(dir.to_str().unwrap()));

        // 别的 MiniApp ⇒ 不能认成自己。
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("package.json"), r#"{"name":"minimax-bot"}"#).unwrap();
        assert_eq!(name_of(&other.join("package.json")).as_deref(), Some("minimax-bot"));
        assert!(!package_is_ours(other.to_str().unwrap()));

        // 坏 JSON ⇒ 认不出（保守：不当成自己）。
        let broken = dir.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("package.json"), "{not json").unwrap();
        assert_eq!(name_of(&broken.join("package.json")), None);
        assert!(!package_is_ours(broken.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_json_anchor_also_identifies_our_package() {
        // ⚠️ 实测：有些运行时目录的 `package.json` **没有 name 字段**
        // （`mavis-miniapp-ZIU9nz` 就是这样），但 `.minimax-plugin/plugin.json` 一定有。
        // ⇒ 只认 package.json 会漏判 ⇒ 判成「运行时没起」⇒ 误报。
        let dir = std::env::temp_dir().join(format!(
            "expiry-guardian-anchor-{}",
            std::process::id()
        ));
        let plugdir = dir.join(".minimax-plugin");
        std::fs::create_dir_all(&plugdir).unwrap();
        // package.json 故意**不写** name。
        std::fs::write(dir.join("package.json"), r#"{"mcode":{}}"#).unwrap();
        std::fs::write(
            plugdir.join("plugin.json"),
            r#"{"name":"expiry-guardian","displayName":"蹬王之王"}"#,
        )
        .unwrap();
        assert!(package_is_ours(dir.to_str().unwrap()), "只靠 plugin.json 也必须认得出来");

        // 别人的包（两个锚点都不是我们）⇒ 不认。
        let other = dir.join("other");
        std::fs::create_dir_all(other.join(".minimax-plugin")).unwrap();
        std::fs::write(other.join("package.json"), r#"{"mcode":{}}"#).unwrap();
        std::fs::write(
            other.join(".minimax-plugin").join("plugin.json"),
            r#"{"name":"minimax-bot"}"#,
        )
        .unwrap();
        assert!(!package_is_ours(other.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_probe_does_not_panic() {
        // 环境相关：只要求「探测本身不崩、不返回 Err」（lsof 存在时）。
        match runtime_alive() {
            Ok(_) => {}
            Err(e) => panic!("lsof 探测失败：{e}"),
        }
    }

    #[test]
    fn stuck_watch_flags_unchanged_backlog() {
        let mut w = StuckWatch::new();
        // 第一次看到 3 ⇒ streak=1
        assert_eq!(w.observe(3, 15), StuckEvent::Started);
        // 之后条数一直不变 ⇒ 第 15 次观测（streak=15）判定为空转。
        for _ in 0..13 {
            assert_eq!(w.observe(3, 15), StuckEvent::None);
        }
        assert_eq!(
            w.observe(3, 15),
            StuckEvent::Stuck { pending: 3, rounds: 15 }
        );
        // 已经报过 ⇒ 不再重复报。
        assert_eq!(w.observe(3, 15), StuckEvent::None);
    }

    #[test]
    fn stuck_watch_recovers_when_backlog_shrinks() {
        let mut w = StuckWatch::new();
        assert_eq!(w.observe(3, 3), StuckEvent::Started);
        assert_eq!(w.observe(3, 3), StuckEvent::None);
        assert_eq!(w.observe(3, 3), StuckEvent::Stuck { pending: 3, rounds: 3 });
        // 条数**变少** ⇒ 真的在消费 ⇒ 恢复。
        assert_eq!(w.observe(2, 3), StuckEvent::Recovered);
        // 恢复后重新开始跟踪（下一个同值只是把 streak 往前推，不重复报「开始」）。
        assert_eq!(w.observe(2, 3), StuckEvent::None);
    }

    #[test]
    fn stuck_watch_does_not_claim_recovery_when_backlog_grows() {
        // ⚠️ 积压**变多**（又挂了新 bot）时**不能**说「已恢复消费」。
        let mut w = StuckWatch::new();
        assert_eq!(w.observe(3, 3), StuckEvent::Started);
        assert_eq!(w.observe(3, 3), StuckEvent::None);
        assert_eq!(w.observe(3, 3), StuckEvent::Stuck { pending: 3, rounds: 3 });
        // 变多 ⇒ 不是恢复。
        assert_eq!(w.observe(5, 3), StuckEvent::Started);
    }

    #[test]
    fn stuck_watch_resets_on_empty_backlog() {
        let mut w = StuckWatch::new();
        assert_eq!(w.observe(3, 2), StuckEvent::Started);
        assert_eq!(w.observe(3, 2), StuckEvent::Stuck { pending: 3, rounds: 2 });
        // 积压清空 ⇒ 恢复。
        assert_eq!(w.observe(0, 2), StuckEvent::Recovered);
        // 回到 0 之后重新开始观察。
        assert_eq!(w.observe(0, 2), StuckEvent::None);
        assert_eq!(w.observe(1, 2), StuckEvent::Started);
    }
}
