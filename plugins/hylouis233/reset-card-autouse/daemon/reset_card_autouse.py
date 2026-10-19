#!/usr/bin/env python3
"""MiniMax Code 重置卡自动使用器。

监控 Token Plan 用量额度，在「额度耗尽、请求开始落到积分计费」的那一刻，
自动使用一张重置卡，把被扣积分的额度重置回来。

实现依据（来自已安装桌面端 MiniMax Inside Code.app 的打包代码，非猜测）：

  额度查询   GET  {BASE}/backend/account/token_plan/remains_percent
             → model_remains[]，quotaType 1=5小时限额 2=周限额 3=视频限额
             → isUnlimited: current_interval_status === 3
             → 耗尽判定沿用桌面端：total>0 且 used/total 达到上限

  重置卡     GET  {BASE}/backend/account/reset_card/list?page=&size=
             POST {BASE}/backend/account/reset_card/use   {source_type, out_biz_no}
  卡历史     GET  {BASE}/backend/account/reset_card/flows

  source_type 1=FIVE_HOUR(5小时限额) 2=WEEKLY(周限额) 3=VIDEO(视频限额)
  outcome     SUCCESS | REPLAYED | EXPIRED | NOT_FOUND | RETRYABLE_FAIL | LOCK_BUSY

  鉴权       Authorization: Bearer <token>（与桌面端同一套登录态）
  业务错误   base_resp.status_code !== 0 时按错误码分派

默认按用户确认的策略执行：
  1. 只在「额度耗尽且已开始扣积分」这一刻扣卡，不提前用
  2. 优先扣 5 小时限额卡；仅当 5 小时限额也耗尽时才动用周限额卡
  3. 跳过已过期卡，只用未过期的

不修改已安装的 app.asar，升级应用不受影响。
"""

from __future__ import annotations

import argparse
import json
import os
import random
import socket
import struct
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

# ── 常量：与桌面端保持一致 ────────────────────────────────────────────────────
# 账号/计费接口的真实根地址（取自桌面端 d5 的生产分支）。
# 注意：不是 agent.minimax.cn —— 那是网页域，对 /backend/account/* 一律 404。
BASE_CN = "https://www.minimaxi.com"
BASE_GLOBAL = "https://platform.minimax.io"
# Code 积分走另一个域名（platform 域对这些路径返回非 JSON）
CODE_BASE_CN = "https://agent.minimaxi.com"
CODE_BASE_GLOBAL = "https://agent.minimax.io"

QUOTA_FIVE_HOUR = 1
QUOTA_WEEKLY = 2
QUOTA_VIDEO = 3

# 判定阈值的**默认值**，仅在 config.json 缺失/非法时兜底。
# 实际生效值一律来自 load_config() —— 与 Node / Rust 读同一个文件。
QUOTA_TRIGGER_PERCENT = 99.0
# 四档红绿灯的两个分界点（功能3）。燃尽档由 used >= total 判定，不可配。
QUOTA_TIGHT_PERCENT = 80.0
QUOTA_LOW_PERCENT = 95.0
# 距 5 小时额度刷新不足这么多分钟时，不消耗重置卡（额度马上就自己回来了）。
# 实际生效值来自 config.json 的 cardSkipBeforeResetMinutes。
QUOTA_CARD_SKIP_BEFORE_RESET_MINUTES = 60.0

# 等级 → 中文。**属于 parity 契约的一部分**，改一个字三方都要改。
LEVEL_LABEL = {
    "comfortable": "额度充足",
    "tight": "额度紧张",
    "low": "额度不足",
    "burned": "额度燃尽",
}

QUOTA_LABEL = {
    QUOTA_FIVE_HOUR: "5小时限额",
    QUOTA_WEEKLY: "周限额",
    QUOTA_VIDEO: "视频限额",
}

# ── 共享配置（功能1/2/3）────────────────────────────────────────────────────
# 与 Mini App 侧、Rust 侧读同一个文件：
#   ~/.config/mcode-reset-card/config.json
CONFIG_PATH = os.path.expanduser("~/.config/mcode-reset-card/config.json")

CONFIG_DIR = os.path.dirname(CONFIG_PATH)


def default_config() -> dict[str, Any]:
    return {
        "schemaVersion": 1,
        "cardTriggerPercent": QUOTA_TRIGGER_PERCENT,
        "cardSkipBeforeResetMinutes": QUOTA_CARD_SKIP_BEFORE_RESET_MINUTES,
        "resumeCronId": "",
        "quotaLevels": {"tight": QUOTA_TIGHT_PERCENT, "low": QUOTA_LOW_PERCENT},
        "goalPause": {"enabled": False, "triggerPercent": QUOTA_TRIGGER_PERCENT,
                      "resumeAtPercent": 0.0},
    }


def _clamp(v: Any, lo: float, hi: float, fallback: float) -> float:
    try:
        f = float(v)
    except (TypeError, ValueError):
        return fallback
    if f != f:  # NaN
        return fallback
    return min(hi, max(lo, f))


def normalize_config(raw: Any) -> tuple[dict[str, Any], list[str]]:
    """逐字段校验并夹紧，返回 (配置, 问题列表)。

    非法组合**整体回退默认并报出来**，绝不静默纠正 ——
    静默纠正会让人以为自己设的生效了。
    """
    d = default_config()
    issues: list[str] = []
    src = raw if isinstance(raw, dict) else {}
    if raw is not None and not isinstance(raw, dict):
        issues.append("配置根节点不是对象")

    raw_card = src.get("cardTriggerPercent")
    card = _clamp(raw_card, 1.0, 100.0, d["cardTriggerPercent"])
    if raw_card is not None:
        try:
            if float(raw_card) != card:
                issues.append(f"cardTriggerPercent={raw_card} 超出 1..100，已夹到 {card}")
        except (TypeError, ValueError):
            issues.append(f"cardTriggerPercent={raw_card} 不是数字，已用默认值 {card}")

    lv = src.get("quotaLevels") if isinstance(src.get("quotaLevels"), dict) else {}
    tight = _clamp(lv.get("tight"), 1.0, 100.0, d["quotaLevels"]["tight"])
    low = _clamp(lv.get("low"), 1.0, 100.0, d["quotaLevels"]["low"])
    if not tight < low:
        if lv:
            issues.append(
                f"quotaLevels 需满足 tight < low（收到 tight={tight} low={low}），已整体回退默认")
        tight = d["quotaLevels"]["tight"]
        low = d["quotaLevels"]["low"]

    gp = src.get("goalPause") if isinstance(src.get("goalPause"), dict) else {}
    raw_skip = src.get("cardSkipBeforeResetMinutes")
    skip = _clamp(raw_skip, 0.0, 1440.0, d["cardSkipBeforeResetMinutes"])
    if raw_skip is not None and skip != _clamp(raw_skip, 0.0, 1440.0, d["cardSkipBeforeResetMinutes"]):
        issues.append(f"cardSkipBeforeResetMinutes={raw_skip} 超出 0..1440，已夹到 {skip}")
    return {
        "schemaVersion": 1,
        "cardTriggerPercent": card,
        "cardSkipBeforeResetMinutes": skip,
        "resumeCronId": str(src.get("resumeCronId") or "").strip(),
        "quotaLevels": {"tight": tight, "low": low},
        "goalPause": {
            "enabled": gp.get("enabled") is True,
            "triggerPercent": _clamp(gp.get("triggerPercent"), 1.0, 100.0,
                                     d["goalPause"]["triggerPercent"]),
            "resumeAtPercent": _clamp(gp.get("resumeAtPercent"), 0.0, 100.0,
                                      d["goalPause"]["resumeAtPercent"]),
        },
    }, issues


def load_config() -> tuple[dict[str, Any], list[str]]:
    """读配置。文件不存在是正常情况（首次运行），返回默认值且不报 issue。"""
    try:
        with open(CONFIG_PATH, encoding="utf-8") as f:
            raw = json.load(f)
    except FileNotFoundError:
        return default_config(), []
    except (OSError, ValueError) as e:
        return default_config(), [f"配置读取失败（{e}），正在用默认值"]
    return normalize_config(raw)

# 桌面端 base_resp 业务错误码 → 语义
ERR_UNAUTHORIZED = 1004
ERR_GROUP_NOT_MEMBER = 1003
ERR_PERMISSION_DENIED = 2029
ERR_INVALID_PARAMS = 2013


class ApiError(RuntimeError):
    """业务或网络错误。"""

    def __init__(self, message: str, *, kind: str = "business", code: int | None = None):
        super().__init__(message)
        self.kind = kind
        self.code = code


def log(msg: str) -> None:
    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{ts}] {msg}", flush=True)


# ── HTTP 客户端 ───────────────────────────────────────────────────────────────
class MatrixClient:
    """最小化的账号 API 客户端，复用桌面端同一套鉴权与错误语义。"""

    def __init__(self, token: str, base: str = BASE_CN, timeout: float = 15.0):
        if not token:
            raise ValueError("token 不能为空")
        self.token = token
        self.base = base.rstrip("/")
        self.timeout = timeout
        # Code 积分接口在另一个域名上，随地域切换
        self.code_base = (CODE_BASE_GLOBAL if self.base == BASE_GLOBAL else CODE_BASE_CN).rstrip("/")

    def _request(self, method: str, path: str, params: dict | None = None,
                 body: dict | None = None, base: str | None = None) -> dict[str, Any]:
        url = f"{base or self.base}{path}"
        if params:
            clean = {k: v for k, v in params.items() if v is not None}
            if clean:
                url = f"{url}?{urllib.parse.urlencode(clean)}"

        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.token}")
        req.add_header("Accept", "application/json")
        req.add_header("User-Agent", "MiniMaxCode")
        if data is not None:
            req.add_header("Content-Type", "application/json")

        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read()
        except urllib.error.HTTPError as exc:
            detail = exc.read()[:400].decode("utf-8", "ignore")
            if exc.code in (401, 403):
                raise ApiError(f"鉴权失败 HTTP {exc.code}，token 可能已过期", kind="unauthorized") from exc
            raise ApiError(f"HTTP {exc.code}: {detail}", kind="network", code=exc.code) from exc
        except urllib.error.URLError as exc:
            raise ApiError(f"网络不可用: {exc.reason}", kind="network") from exc

        try:
            payload = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise ApiError(f"响应不是合法 JSON: {raw[:200]!r}", kind="network") from exc

        # 桌面端：base_resp.status_code !== 0 即业务错误
        base_resp = payload.get("base_resp")
        if isinstance(base_resp, dict) and base_resp.get("status_code", 0) != 0:
            code = int(base_resp.get("status_code", 0))
            msg = base_resp.get("status_msg") or base_resp.get("msg") or f"code={code}"
            kind = {
                ERR_UNAUTHORIZED: "unauthorized",
                ERR_GROUP_NOT_MEMBER: "group",
                ERR_PERMISSION_DENIED: "permission",
                ERR_INVALID_PARAMS: "invalid-params",
            }.get(code, "business")
            raise ApiError(f"{msg} (code={code})", kind=kind, code=code)

        return payload

    # ── 账号接口 ─────────────────────────────────────────────────────────────
    def remains_percent(self) -> dict[str, Any]:
        return self._request("GET", "/backend/account/token_plan/remains_percent")

    def platform_credit(self) -> dict[str, Any]:
        """平台积分池（含 balance_breakdown：各池余额与到期时间）。"""
        return self._request("GET", "/backend/account/token_plan_credit")

    def code_credit_details(self, page: int = 1, page_size: int = 100) -> dict[str, Any]:
        """Code 积分逐笔明细。

        参数名是 page_number / page_size（下划线），且 include_expired=1 才会
        返回历史记录 —— 桌面端就是这样调的。page_size 上限 100。
        """
        return self._request(
            "GET",
            f"/minimax-cloud/api/v1/credit/details?page_number={page}&page_size={page_size}&include_expired=1",
            base=self.code_base,
        )

    def membership_info(self) -> dict[str, Any]:
        """账户积分汇总（op_credit_summary），POST。"""
        return self._request("POST", "/matrix/api/v1/commerce/get_membership_info", body={}, base=self.code_base)

    def list_reset_cards(self, page: int = 1, size: int = 20) -> list[dict[str, Any]]:
        data = self._request("GET", "/backend/account/reset_card/list",
                             params={"page": page, "size": size})
        cards = data.get("cards") or []
        return cards if isinstance(cards, list) else []

    def use_reset_card(self, source_type: str, out_biz_no: str) -> str:
        """返回 outcome。

        source_type 与 out_biz_no 都取自卡对象本身（服务端下发的字符串，
        如 'mcode_signin' / 'mcode_admin_benefit'），原样透传，不做本地枚举映射。
        out_biz_no 由服务端为每张卡生成，同时充当幂等键。
        """
        data = self._request("POST", "/backend/account/reset_card/use",
                             body={"source_type": source_type, "out_biz_no": out_biz_no})
        return str(data.get("outcome") or "UNKNOWN")

    def reset_card_flows(self, page: int = 1, size: int = 20) -> list[dict[str, Any]]:
        data = self._request("GET", "/backend/account/reset_card/flows",
                             params={"page": page, "size": size})
        flows = data.get("flows") or []
        return flows if isinstance(flows, list) else []


# ── 额度判定 ──────────────────────────────────────────────────────────────────
@dataclass
class QuotaState:
    """一个限额维度的用量状态。"""

    quota_type: int
    total: float = 0.0
    used: float = 0.0
    is_unlimited: bool = False
    reset_time: int | None = None

    def is_triggered(self, trigger_percent: float) -> bool:
        """是否达到扣卡触发线。

        阈值必须**显式传入** —— 写死在本类里正是当初三方漂移的根源：
        配置改了，这里还按 99 算，而且不报错。
        """
        if self.is_unlimited or self.total <= 0.0:
            return False
        return self.used >= self.total * trigger_percent / 100.0

    def level(self, tight: float = QUOTA_TIGHT_PERCENT,
              low: float = QUOTA_LOW_PERCENT) -> str:
        """四档红绿灯分档。与 Rust ``quota_level()`` 逐行对应。

        燃尽档由 used >= total 判定，**不可配** —— 它是事实，不是阈值。
        """
        if self.is_unlimited or self.total <= 0.0:
            return "comfortable"
        if self.used >= self.total:
            return "burned"
        ratio = self.used / self.total * 100.0
        if ratio >= low:
            return "low"
        if ratio >= tight:
            return "tight"
        return "comfortable"

    @property
    def exhausted(self) -> bool:
        """是否已耗尽。

        接口语义（对齐桌面端 V/q 两个转换函数）：
          used  = *_used_percent   → **已用百分比**，如 "71%" → 71
          total = *_total_percent  → 总额度百分比，缺省按 100 计

        所以耗尽就是「已用百分比 ≥ 总额度百分比」。不限量（status===3）不算耗尽。
        """
        if self.is_unlimited or self.total <= 0:
            return False
        return self.used >= self.total - 1e-9

    @property
    def remaining_percent(self) -> float:
        if self.is_unlimited or self.total <= 0:
            return 100.0
        return max(0.0, (self.total - self.used) / self.total * 100.0)

    def describe(self) -> str:
        if self.is_unlimited:
            return f"{QUOTA_LABEL.get(self.quota_type, self.quota_type)}=不限量"
        return (f"{QUOTA_LABEL.get(self.quota_type, self.quota_type)}="
                f"已用{self.used:.0f}% / 剩余{self.remaining_percent:.0f}%")


def _num(value: Any, default: float = 0.0) -> float:
    """把接口字段转成数字。

    关键：`*_percent` 字段返回的是**带百分号的字符串**，例如 "71%" / "100%"，
    不是数字。直接 float() 会抛 ValueError 并静默退回 0，导致把
    「已用 71%」误判成「已用 0% / 剩余 100%」——曾因此漏判额度耗尽。
    这里与桌面端一致，用 parseFloat(v.replace(/%/g,''))。
    """
    if isinstance(value, str):
        value = value.strip().replace("%", "")
    if value is None or value == "":
        return default
    try:
        f = float(value)
        return f if f == f else default  # 过滤 NaN
    except (TypeError, ValueError):
        return default


def parse_quotas(payload: dict[str, Any]) -> list[QuotaState]:
    """按桌面端 queryPercent 的解析逻辑提取 5小时/周/视频限额。"""
    models = payload.get("model_remains") or []
    quotas: list[QuotaState] = []

    general = next((m for m in models if isinstance(m, dict) and m.get("model_name") == "general"), None)
    if general is None:
        general = next((m for m in models if isinstance(m, dict) and m.get("model_name")), None)

    if general:
        quotas.append(QuotaState(
            quota_type=QUOTA_FIVE_HOUR,
            total=_num(general.get("current_interval_total_percent")),
            used=_num(general.get("current_interval_used_percent")),
            is_unlimited=general.get("current_interval_status") == 3,
            reset_time=general.get("end_time"),
        ))
        quotas.append(QuotaState(
            quota_type=QUOTA_WEEKLY,
            total=_num(general.get("current_weekly_total_percent")),
            used=_num(general.get("current_weekly_used_percent")),
            is_unlimited=general.get("current_weekly_status") == 3,
            reset_time=general.get("weekly_end_time"),
        ))

    video = next((m for m in models
                  if isinstance(m, dict)
                  and m.get("model_name") != "general"
                  and _num(m.get("current_interval_total_count")) > 0), None)
    if video:
        quotas.append(QuotaState(
            quota_type=QUOTA_VIDEO,
            total=_num(video.get("current_interval_total_count")),
            used=_num(video.get("current_interval_usage_count")),
            is_unlimited=video.get("current_interval_status") == 3,
            reset_time=video.get("end_time"),
        ))

    return quotas


# ── 核心：自动扣一张卡 ────────────────────────────────────────────────────────
STATE_DIR = os.path.expanduser("~/.config/mcode-reset-card")
STATE_FILE = os.path.join(STATE_DIR, "state.json")


@dataclass
class AutoUseStats:
    used_count: int = 0
    skipped_no_card: int = 0
    skipped_not_exhausted: int = 0
    errors: int = 0
    seen_fallback: set[str] = field(default_factory=set)


def load_state() -> dict[str, Any]:
    """读取持久化状态：记录已针对哪个耗尽组合扣过卡。

    必须持久化：只在内存里去重的话，进程重启就会重复扣同一张卡。
    """
    try:
        with open(STATE_FILE, encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else {}
    except (OSError, json.JSONDecodeError):
        return {}


def save_state(data: dict[str, Any]) -> None:
    os.makedirs(STATE_DIR, mode=0o700, exist_ok=True)
    tmp = f"{STATE_FILE}.tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(data, fh, ensure_ascii=False, indent=2)
    os.replace(tmp, STATE_FILE)
    os.chmod(STATE_FILE, 0o600)


def _pick_card(cards: list[dict[str, Any]], now_ms: int) -> dict | None:
    """挑一张未过期的卡（优先最早到期，避免临期浪费）。

    不在本地按限额维度过滤：卡自带的 source_type 是服务端下发的字符串
    （如 'mcode_signin'），与哪个限额重置的对应关系由服务端判定。
    """
    usable = []
    for card in cards:
        if not isinstance(card, dict):
            continue
        expire = card.get("expire_time")
        # 过期卡直接跳过，避免必然失败的 EXPIRED
        if isinstance(expire, (int, float)) and expire <= now_ms:
            continue
        usable.append(card)

    if not usable:
        return None
    usable.sort(key=lambda c: (c.get("expire_time") or 2**53, c.get("create_time") or 0))
    return usable[0]


def fetch_all_cards(client: "Session | MatrixClient", max_pages: int = 10) -> list[dict[str, Any]]:
    """翻页拉取全部重置卡（桌面端最多翻到 50 页）。

    同时接受 Session（自动续期）与裸 MatrixClient，两种调用方都能用。
    """
    def _list(page: int, size: int) -> list[dict[str, Any]]:
        if isinstance(client, MatrixClient):
            return client.list_reset_cards(page=page, size=size)
        return client.call("list_reset_cards", page=page, size=size)

    seen: dict[str, dict] = {}
    for page in range(1, max_pages + 1):
        cards = _list(page, 20)
        if not cards:
            break
        for card in cards:
            if isinstance(card, dict):
                cid = card.get("card_id")
                if cid:
                    seen[cid] = card
        if len(cards) < 20:
            break
    return list(seen.values())


def try_auto_use(client: Session, stats: AutoUseStats,
                 *, dry_run: bool = False) -> bool:
    """检测额度耗尽并自动扣一张卡。返回是否发生了扣卡动作。

    策略（按用户确认）：
      - 只在额度耗尽这一刻扣，不提前用
      - 跳过已过期卡，只用未过期的
      - 失败不无限重试

    client 传 Session 而非裸 MatrixClient，以便 token 过期时自动续期。
    """
    quotas = parse_quotas(client.call("remains_percent"))
    by_type = {q.quota_type: q for q in quotas}

    five_hour = by_type.get(QUOTA_FIVE_HOUR)
    weekly = by_type.get(QUOTA_WEEKLY)

    five_exhausted = bool(five_hour and five_hour.exhausted)
    weekly_exhausted = bool(weekly and weekly.exhausted)

    log("额度状态: " + " | ".join(q.describe() for q in quotas))

    # 「已开始扣积分」= 至少一个限额耗尽
    if not (five_exhausted or weekly_exhausted):
        stats.skipped_not_exhausted += 1
        return False

    # 选卡优先级：先 5 小时；5 小时与周限额都耗尽时，周限额也可以用
    candidates: list[int] = []
    if five_exhausted:
        candidates.append(QUOTA_FIVE_HOUR)
    if weekly_exhausted and five_exhausted:
        candidates.append(QUOTA_WEEKLY)

    if not candidates:
        stats.skipped_no_card += 1
        return False

    # 耗尽轮次标识 = 限额组合 + 各自的重置时间。
    # 只用「组合」去重会永久封死：额度恢复后再次耗尽会被误判为已扣过。
    # 带上 reset_time 后，同一轮耗尽只扣一张卡，跨轮（新 5 小时窗口）才允许再扣。
    cycle = "|".join(
        f"{q.quota_type}:{q.reset_time or 0}" for q in quotas
        if q.quota_type in (QUOTA_FIVE_HOUR, QUOTA_WEEKLY)
    )
    fingerprint = f"{cycle}#5h={'1' if five_exhausted else '0'},7d={'1' if weekly_exhausted else '0'}"
    state = load_state()
    spent = set(state.get("spent_fingerprints") or [])
    if fingerprint in spent:
        return False
    stats.seen_fallback.add(fingerprint)

    now_ms = int(time.time() * 1000)
    cards = fetch_all_cards(client)
    live = [c for c in cards
            if isinstance(c.get("expire_time"), (int, float)) and c["expire_time"] > now_ms]
    log(f"重置卡: 共 {len(cards)} 张，未过期 {len(live)} 张")

    exhausted_names = "、".join(QUOTA_LABEL.get(t, str(t)) for t in candidates)
    card = _pick_card(cards, now_ms)
    if card is None:
        log(f"无可用重置卡，跳过（{exhausted_names} 已耗尽）")
        stats.skipped_no_card += 1
        return False

    card_id = card.get("card_id")
    source_type = str(card.get("source_type") or "")
    out_biz_no = str(card.get("out_biz_no") or "")
    expire = card.get("expire_time")
    expire_txt = time.strftime("%Y-%m-%d %H:%M", time.localtime(expire / 1000)) \
        if isinstance(expire, (int, float)) else "未知"

    if not source_type or not out_biz_no:
        log(f"✗ 卡 {card_id} 缺少 source_type/out_biz_no，跳过")
        stats.skipped_no_card += 1
        return False

    if dry_run:
        log(f"[dry-run] 本应使用重置卡 {card_id}（{source_type}，{expire_txt} 到期），已跳过实际扣卡")
        stats.used_count += 1
        return True

    log(f"额度耗尽（{exhausted_names}），自动使用重置卡 {card_id}（{source_type}，{expire_txt} 到期）…")

    # 先落盘再请求：进程中途崩溃也不会把这张卡再扣一次。
    # out_biz_no 用服务端下发的值，天然充当幂等键。
    spent.add(fingerprint)
    state["spent_fingerprints"] = sorted(spent)
    history = list(state.get("history") or [])
    history.append({
        "at": now_ms, "card_id": card_id, "source_type": source_type,
        "out_biz_no": out_biz_no, "fingerprint": fingerprint,
        "expire_time": expire,
    })
    state["history"] = history[-100:]
    save_state(state)

    try:
        outcome = client.call("use_reset_card", source_type, out_biz_no)
    except ApiError as exc:
        if exc.kind == "network":
            # 网络错误：结果未知，重试会用同一张卡，不会重复扣卡
            log(f"⚠️  扣卡结果未知（网络错误）：{exc}。稍后重试会复用同一张卡，不会重复扣卡。")
        else:
            log(f"✗ 扣卡失败：{exc}")
        stats.errors += 1
        return False

    if outcome in ("SUCCESS", "REPLAYED"):
        log(f"✓ 重置卡使用成功（outcome={outcome}），额度已重置")
        stats.used_count += 1
        return True
    if outcome in ("EXPIRED", "NOT_FOUND"):
        log(f"✗ 重置卡不可用（outcome={outcome}），跳过")
        stats.skipped_no_card += 1
        return False
    if outcome == "LOCK_BUSY":
        log(f"⏳ 重置卡被占用（outcome=LOCK_BUSY），下一轮重试")
        stats.errors += 1
        return False
    log(f"⏳ 重置卡可重试失败（outcome={outcome}），下一轮重试")
    stats.errors += 1
    return False


# ── 命令 ──────────────────────────────────────────────────────────────────────
AUTH_CAP_FILE = os.path.expanduser("~/.minimax/run/mcode-auth-lease-v1.cap")
AUTH_SOCK_FILE = os.path.expanduser("~/.minimax/run/mcode-auth-lease-v1.sock")
AUTH_TOKEN_FILE = os.path.expanduser("~/.config/mcode-reset-card/token")


def acquire_lease_token(min_validity_ms: int = 120_000) -> str | None:
    """通过桌面端的 OAuth lease socket 取当前登录态的 access token。

    这是与桌面端完全相同的凭据来源，无需手工复制粘贴。token 有效期较短
    （约 40 分钟），因此每次需要调用前重新获取，而不是长期缓存。

    需要桌面端正在运行（它才是 lease 的服务端）。协议见
    packages/oauth-lease-protocol：4 字节大端长度前缀 + JSON。
    """
    if not (os.path.exists(AUTH_SOCK_FILE) and os.path.exists(AUTH_CAP_FILE)):
        return None
    try:
        with open(AUTH_CAP_FILE, encoding="utf-8") as fh:
            capability = fh.read().strip()
    except OSError:
        return None
    if not capability:
        return None

    payload = json.dumps({
        "version": 1,
        "requestId": uuid.uuid4().hex,
        "capability": capability,
        "method": "lease",
        "minValidityMs": min_validity_ms,
    }).encode()
    frame = struct.pack(">I", len(payload)) + payload

    sock = None
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect(AUTH_SOCK_FILE)
        sock.sendall(frame)

        buf = b""
        while len(buf) < 4:
            chunk = sock.recv(65536)
            if not chunk:
                return None
            buf += chunk
        size = struct.unpack(">I", buf[:4])[0]
        while len(buf) < 4 + size:
            chunk = sock.recv(65536)
            if not chunk:
                break
            buf += chunk
        resp = json.loads(buf[4:4 + size].decode("utf-8"))
    except (OSError, ValueError, struct.error):
        return None
    finally:
        if sock is not None:
            sock.close()

    if not resp.get("ok"):
        return None
    result = resp.get("result") or {}
    if result.get("method") == "unauthorized":
        return None
    if result.get("method") != "lease":
        return None
    token = result.get("accessToken")
    return token if isinstance(token, str) and token else None


def persist_token(token: str) -> None:
    """把 token 存到 600 权限文件，仅供人工排查使用。"""
    os.makedirs(os.path.dirname(AUTH_TOKEN_FILE), mode=0o700, exist_ok=True)
    with open(AUTH_TOKEN_FILE, "w", encoding="utf-8") as fh:
        fh.write(token)
    os.chmod(AUTH_TOKEN_FILE, 0o600)


def resolve_token(*, refresh: bool = True) -> str:
    """解析 token，优先从桌面端 lease 实时获取，其次环境变量，最后本地文件。

    refresh=True 时会先试 lease：这样 token 过期后能自动续上，无需人工干预。
    """
    if refresh:
        token = acquire_lease_token()
        if token:
            return token

    for env in ("MCODE_TOKEN", "MINIMAX_TOKEN"):
        val = os.environ.get(env, "").strip()
        if val:
            return val
    if os.path.isfile(AUTH_TOKEN_FILE):
        with open(AUTH_TOKEN_FILE, encoding="utf-8") as fh:
            val = fh.read().strip()
            if val:
                return val

    raise SystemExit(
        "无法获取登录 token。请确保 MiniMax Code 桌面端正在运行（它提供登录态），\n"
        "或设置环境变量 MCODE_TOKEN / MINIMAX_TOKEN。\n"
        "详见 README「获取 token」。"
    )


class Session:
    """按需刷新 token 的客户端封装。

    token 约 40 分钟过期，所以遇到鉴权失败时自动重新取 lease 再重试一次，
    这是实现真正无人值守的关键。
    """

    def __init__(self, base: str):
        self.base = base
        self.token = resolve_token()
        self.client = MatrixClient(self.token, base=base)

    def _refresh(self) -> None:
        self.token = resolve_token()
        self.client = MatrixClient(self.token, base=self.base)

    def call(self, fn_name: str, *args, **kwargs):
        """执行一次 API 调用；鉴权失败时刷新 token 重试一次。"""
        try:
            return getattr(self.client, fn_name)(*args, **kwargs)
        except ApiError as exc:
            if exc.kind not in ("unauthorized",) and exc.code not in (1004, 1016):
                raise
            log(f"凭据失效（{exc}），正在从桌面端刷新…")
            self._refresh()
            return getattr(self.client, fn_name)(*args, **kwargs)


def cmd_status(session: Session) -> int:
    quotas = parse_quotas(session.call("remains_percent"))
    print("── 额度状态 ──")
    for q in quotas:
        mark = "已耗尽 ⚠️" if q.exhausted else "可用"
        reset_txt = ""
        if q.reset_time:
            reset_txt = f"  (重置于 {time.strftime('%m-%d %H:%M', time.localtime(q.reset_time / 1000))})"
        print(f"  {q.describe():<28} {mark}{reset_txt}")
    print("── 重置卡 ──")
    now = int(time.time() * 1000)
    try:
        cards = fetch_all_cards(session)
    except ApiError as exc:
        print(f"  拉取失败：{exc}")
        return 1
    if not cards:
        print("  无重置卡")
    for card in cards:
        expire = card.get("expire_time")
        expire_txt = time.strftime("%Y-%m-%d %H:%M", time.localtime(expire / 1000)) \
            if isinstance(expire, (int, float)) else "未知"
        state = "未过期" if isinstance(expire, (int, float)) and expire > now else "已过期"
        label = QUOTA_LABEL.get(card.get("source_type"), card.get("source_type"))
        print(f"  {card.get('card_id')}  {str(label):<10} {expire_txt}  {state}")

    history = load_state().get("history") or []
    if history:
        print("── 本工具自动扣卡记录 ──")
        for item in history[-10:]:
            label = QUOTA_LABEL.get(item.get("source_type"), item.get("source_type"))
            when = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(item["at"] / 1000))
            print(f"  {when}  {label}  card={item.get('card_id')}")
    return 0


def cmd_once(client: MatrixClient, args) -> int:
    try:
        result = background_tick(client, dry_run=True)
    except ApiError as exc:
        print(f"失败：{exc}", file=sys.stderr)
        return 1
    print(f"动作：{result['action']}")
    print(f"理由：{result['reason']}")
    return 0


# ── 后台无人值守决策 ──────────────────────────────────────────────────────────
# 策略与 Mini App「蹬王之王」保持完全一致：同样只在**真正耗尽**时才动手，
# 同样按「先过期先用」决定用卡还是留卡。
#
# 保留这两个常量只为可读性与将来微调，但判定逻辑统一走 QuotaState.exhausted
# （used >= total，即 100%），不再有 90% 这类提前量。
TRIGGER_USED_PERCENT = 99.0
WEEKLY_TRIGGER_PERCENT = 100.0

BUCKET_LABEL = {2: "充值", 3: "赠送"}


def _amount(value: Any, default: float = 0.0) -> float:
    if isinstance(value, str):
        value = value.replace(",", "").strip()
    try:
        n = float(value)
        return n if n == n else default
    except (TypeError, ValueError):
        return default


def _fmt_day(ms: int | None) -> str:
    if not ms:
        return "未知"
    d = datetime.fromtimestamp(ms / 1000)
    return f"{d.month}月{d.day}日"


def earliest_credit_expiry(client: "Session | MatrixClient") -> tuple[int | None, str]:
    """全账户里最早到期的一笔「仍有效且有余额」的积分。

    两个来源取更早的：平台积分池 + Code 积分明细。
    单个来源失败不影响另一个，宁可少算也不编造。
    """
    now = int(time.time() * 1000)
    best: int | None = None
    desc = ""

    def consider(exp: Any, balance: Any, label: str) -> None:
        nonlocal best, desc
        if not isinstance(exp, (int, float)) or exp <= now:
            return
        if balance <= 0:
            return
        if best is None or exp < best:
            best = int(exp)
            desc = label

    try:
        credit = client.call("platform_credit") if isinstance(client, Session) else client.platform_credit()
        for b in (credit.get("balance_breakdown") or {}).get("buckets", []) or []:
            if not isinstance(b, dict):
                continue
            consider(b.get("expire_time_ms"), _amount(b.get("balance")),
                     f"平台积分·{BUCKET_LABEL.get(b.get('source'), '未知')}")
    except Exception as exc:  # noqa: BLE001
        log(f"  平台积分到期读取失败（忽略）: {exc}")

    try:
        raw = client.call("code_credit_details") if isinstance(client, Session) else client.code_credit_details()
        for item in (raw or {}).get("details") or []:
            if not isinstance(item, dict):
                continue
            label = "Code 积分·赠送" if item.get("credit_type") == 2 else "Code 积分·购买"
            consider(item.get("expire_at_ms"), _amount(item.get("remaining_amount")), label)
    except Exception as exc:  # noqa: BLE001
        log(f"  Code 积分到期读取失败（忽略）: {exc}")

    return best, desc


def _minutes_to_reset(q: "QuotaState | None", now_ms: int) -> float | None:
    """某个额度维度距离刷新还有多少分钟。取不到刷新时间时返回 None。

    与 Rust 的 `minutes_to_reset()` 同一口径：已过去的时间按 0 算。
    """
    if q is None or q.reset_time is None:
        return None
    return max(0.0, (q.reset_time - now_ms) / 60_000.0)


def decide_background(
    quotas: list[QuotaState],
    usable_cards: list[dict[str, Any]],
    credit_expiry_ms: int | None,
    credit_desc: str,
    cfg: dict[str, Any] | None = None,
    now_ms: int | None = None,
) -> tuple[str, str]:
    """后台扣卡决策，返回 (动作, 理由)。

    动作 ∈ {"use-card", "keep-credit", "idle", "wait"}。纯函数，不触碰网络，可直接单测。

    **分支与 Mini App「蹬王之王」的 decide() 一一对应**，刻意保持同构：

      未耗尽                     → idle        （额度等级 + 已用量 + 未到触发线）
      耗尽 + 无积分 + 有卡        → use-card    （没有可用积分，用卡重置额度）
      耗尽 + 无积分 + 无卡        → wait
      耗尽 + 有积分 + 无卡        → keep-credit （没卡可用，让积分先消耗）
      耗尽 + 都有，卡先过期        → use-card    （先用卡保住积分）
      耗尽 + 都有，积分先过期      → keep-credit （让积分先消耗，留住更耐放的卡）

    `cfg` 省略时从共享配置文件读取（与 Node / Rust 读同一个文件）。
    `now_ms` 用于判断「额度是不是马上就要刷新了」。
    """
    if cfg is None:
        cfg, _issues = load_config()
    if now_ms is None:
        now_ms = int(time.time() * 1000)

    trigger = float(cfg["cardTriggerPercent"])
    tight = float(cfg["quotaLevels"]["tight"])
    low = float(cfg["quotaLevels"]["low"])
    skip_minutes = float(cfg.get("cardSkipBeforeResetMinutes", QUOTA_CARD_SKIP_BEFORE_RESET_MINUTES))

    by_type = {q.quota_type: q for q in quotas}
    five = by_type.get(QUOTA_FIVE_HOUR)
    weekly = by_type.get(QUOTA_WEEKLY)

    five_exhausted = bool(five and five.is_triggered(trigger))
    weekly_exhausted = bool(weekly and weekly.is_triggered(trigger))

    # ── 5 小时额度即将自己刷新 → 不烧重置卡（纯浪费）──
    reset_in_min = _minutes_to_reset(five, now_ms)
    skip_card_reason = None
    if skip_minutes > 0 and reset_in_min is not None and reset_in_min <= skip_minutes:
        skip_card_reason = f"5 小时额度 {reset_in_min:.0f} 分钟后刷新，暂不消耗重置卡"

    # ── 未到触发线：与 Mini App 一致，不做任何事 ──
    if not five_exhausted and not weekly_exhausted:
        used5 = five.used if five is not None else 0.0
        # 等级词随分档走，而不是永远写「额度充足」——
        # 否则用量 92% 时页面上仍显示「额度充足」，是误导。
        # 把触发线也说进来：否则「额度不足但暂不需要处理」看起来自相矛盾。
        # burned 永远不会出现在这里：它必然已 triggered，走的是下面用卡/留卡分支。
        level = five.level(tight, low) if five is not None else "comfortable"
        return "idle", (
            f"{LEVEL_LABEL[level]}（5 小时已用 {used5:.0f}%），"
            f"未到 {trigger:.0f}% 触发线，暂不动用重置卡"
        )

    # ── 无可用积分：只能靠卡把额度重置回来 ──
    if credit_expiry_ms is None:
        if usable_cards:
            if skip_card_reason:
                return "wait", skip_card_reason
            return "use-card", "没有可用积分，使用重置卡重置额度"
        return "wait", "额度已耗尽，既没有积分也没有可用的重置卡"

    # ── 有积分但没有可用卡：让积分先消耗 ──
    if not usable_cards:
        return "keep-credit", (
            f"没有可用的重置卡，让{credit_desc}（{_fmt_day(credit_expiry_ms)}到期）先消耗"
        )

    card_expiry = usable_cards[0].get("expire_time")
    if card_expiry is None:
        return "keep-credit", "没有可用重置卡，让积分先消耗"

    # ── 两者都有：比「先过期先用」 ──
    if card_expiry < credit_expiry_ms:
        if skip_card_reason:
            return "wait", skip_card_reason
        return "use-card", (
            f"重置卡（{_fmt_day(card_expiry)}）比{credit_desc}（{_fmt_day(credit_expiry_ms)}）先过期，"
            f"先用卡保住积分"
        )
    return "keep-credit", (
        f"{credit_desc}（{_fmt_day(credit_expiry_ms)}）比重置卡（{_fmt_day(card_expiry)}）先过期，"
        f"不用卡，让积分先消耗"
    )


def background_tick(client: "Session | MatrixClient", *, dry_run: bool = False) -> dict[str, Any]:
    """后台一轮：判定 + （必要时）扣卡。返回本轮结果摘要，便于写日志。"""
    quotas = parse_quotas(
        client.call("remains_percent") if isinstance(client, Session) else client.remains_percent()
    )
    now = int(time.time() * 1000)

    cards = fetch_all_cards(client)
    # 与 Mini App 的 liveCards 过滤完全一致：未过期 + 有 out_biz_no + 有 source_type
    usable = [
        c for c in cards
        if c.get("expire_time") is not None
        and c["expire_time"] > now
        and c.get("out_biz_no")
        and c.get("source_type")
    ]
    usable.sort(key=lambda c: c.get("expire_time") or float("inf"))

    credit_expiry, credit_desc = earliest_credit_expiry(client)
    cfg, cfg_issues = load_config()
    for _s in cfg_issues:
        log(f"⚠️  {_s}")
    action, reason = decide_background(quotas, usable, credit_expiry, credit_desc, cfg, now)

    log("额度: " + " | ".join(q.describe() for q in quotas))
    log(f"决策[{action}]: {reason}")

    result = {"action": action, "reason": reason, "at": int(time.time()), "used": False}
    if action != "use-card" or dry_run:
        return result

    card = usable[0]
    fingerprint = f"{card.get('card_id')}:{card.get('out_biz_no')}"
    state = background_state()
    if fingerprint in state["spent"]:
        log("这张卡本进程已尝试过，跳过")
        return result

    if dry_run:
        log(f"[dry-run] 本应使用重置卡 {card.get('card_id')}，已跳过实际扣卡")
        result["used"] = True
        return result

    # 先记账再请求：崩溃也不会重复扣
    state["spent"].add(fingerprint)
    persist_background_state(state)
    try:
        outcome = (client.call("use_reset_card", str(card["source_type"]), str(card["out_biz_no"]))
                   if isinstance(client, Session)
                   else client.use_reset_card(str(card["source_type"]), str(card["out_biz_no"])))
    except ApiError as exc:
        log(f"✗ 扣卡失败：{exc}")
        return result

    if outcome in ("SUCCESS", "REPLAYED"):
        log(f"✓ 已使用重置卡 {card.get('card_id')}，额度已重置")
        result["used"] = True
    else:
        log(f"✗ 重置卡不可用（{outcome}）")
    return result


BG_STATE_FILE = os.path.expanduser("~/.config/mcode-reset-card/bg-state.json")


def background_state() -> dict[str, Any]:
    """后台去重记录落盘，进程重启也不会重复扣同一张卡。"""
    try:
        with open(BG_STATE_FILE, encoding="utf-8") as fh:
            data = json.load(fh)
        if isinstance(data, dict):
            data.setdefault("spent", [])
            data["spent"] = list(data.get("spent") or [])
            return data
    except (OSError, json.JSONDecodeError, AttributeError):
        pass
    return {"spent": []}


def persist_background_state(data: dict[str, Any]) -> None:
    os.makedirs(os.path.dirname(BG_STATE_FILE), mode=0o700, exist_ok=True)
    tmp = BG_STATE_FILE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(data, fh, ensure_ascii=False, indent=2)
    os.replace(tmp, BG_STATE_FILE)
    os.chmod(BG_STATE_FILE, 0o600)


def cmd_watch(session: Session, args) -> int:
    log(f"启动后台监控，间隔 {args.interval}s（Ctrl+C 退出）")
    log("扣卡策略：与 Mini App「蹬王之王」一致 —— 额度真正耗尽才动手，并按先过期先用")
    while True:
        try:
            background_tick(session, dry_run=args.dry_run)
        except ApiError as exc:
            if exc.kind == "unauthorized":
                # Session 已自动重试过一次仍失败，多半是桌面端没登录/没运行
                log(f"✗ 鉴权失败：{exc}。请确认 MiniMax Code 桌面端已启动并处于登录状态。")
                return 1
            log(f"⚠️  本轮失败：{exc}，下一轮继续")
        except KeyboardInterrupt:
            log("已停止")
            return 0
        except Exception as exc:  # noqa: BLE001 - 后台长跑不能让单轮异常终止进程
            log(f"⚠️  本轮异常：{type(exc).__name__}: {exc}，下一轮继续")
        # 抖动，避免与整点/整 5 小时重置撞在一起
        time.sleep(max(5.0, args.interval + random.uniform(-2, 2)))


def main() -> int:
    parser = argparse.ArgumentParser(
        description="MiniMax Code 重置卡自动使用器",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--base", default=os.environ.get("MCODE_BASE", BASE_CN),
                        help="API 根地址（默认国内版 https://www.minimaxi.com）")
    parser.add_argument("--interval", type=float, default=60.0,
                        help="watch 模式轮询间隔秒数（默认 60）")
    parser.add_argument("--dry-run", action="store_true",
                        help="只检测不扣卡，打印本应执行的动作")
    sub = parser.add_subparsers(dest="command")
    sub.add_parser("status", help="打印额度与重置卡状态")
    sub.add_parser("once", help="检测一次，必要时扣一张卡")
    sub.add_parser("watch", help="常驻监控，自动扣卡")

    reset_parser = sub.add_parser("forget", help="清空本地去重记录（不会动服务器上的卡）")
    reset_parser.add_argument("--all", action="store_true",
                              help="同时清空扣卡历史，只保留去重记录")

    args = parser.parse_args()
    command = args.command or "once"

    # forget 不需要 token
    if command == "forget":
        state = load_state()
        state["spent_fingerprints"] = []
        if getattr(args, "all", False):
            state["history"] = []
        save_state(state)
        print("已清空本地去重记录。")
        return 0

    try:
        session = Session(base=args.base)
    except (ValueError, SystemExit) as exc:
        print(str(exc), file=sys.stderr)
        return 1

    if command == "status":
        return cmd_status(session)
    if command == "watch":
        return cmd_watch(session, args)
    return cmd_once(session, args)


if __name__ == "__main__":
    sys.exit(main())
