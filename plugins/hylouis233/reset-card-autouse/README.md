# 蹬王之王 · Reset Card Auto-Use

Track when your MiniMax Code credits and reset cards expire, and let the machine burn
them before they go to waste.

The Mini App is the dashboard. The background daemon is the executor: it runs headless
under `launchd`, pauses your goals when the 5-hour quota runs out, spends a reset card
when that is the right move, and resumes the goals once the quota comes back.

## What it does

| | |
| --- | --- |
| **Expiry radar** | Lists every credit and reset card with its expiry date, so you can see what is about to be wasted. |
| **Goal auto-pause** | When the 5-hour quota crosses the trigger line (default 95%), every in-flight goal is paused automatically. |
| **Goal auto-resume** | When the quota falls back below the trigger, the goals are resumed *and actually woken up* — not just flipped to `active`. |
| **Card selection** | "Expiring first" policy: if the card expires before the credits, spend the card; if the credits expire first, let the credits burn first and keep the card. |
| **Headless** | The daemon keeps running with MiniMax Code closed, after logout, and while the machine sleeps. |

### v1.6 — silent failures made loud (2026-10-20, night)

The theme: **stop letting "everything looks fine" coexist with "not a single Bot was
actually watched or woken"** — and fix three real bugs found in production.

| Change | Effect |
| --- | --- |
| **Lenient JSON parsing for the Bot roster** | `minimax-bot`'s `state.json` contains a **truncated surrogate escape** (`\ud83d`). Python and Node parse it; **Rust's `serde_json` rejects it** ⇒ the roster came back empty ⇒ the **entire Bot watchdog went blind** — the daemon ran, the log was clean, and **zero Bots were being watched**. Measured: before the fix, 4 restarts over 45 minutes produced **no watchdog log lines at all**; after it, "checked 45" immediately |
| **Judge "is it broken?" by the latest *turn*, not by the session's `error_code`** | `error_code` is a **stale value** (the runtime never clears it). Measured on 2026-10-20 15:39: the old rule wrote **11** wake-up requests, of which **8 were false positives** — those 8 Bots' latest turn was `completed` (perfectly healthy); the error code was left over from an earlier failure. After the fix the same batch reported "0 self-healing, 0 dispatched", and only the **2 genuinely broken** Bots were woken |
| **Acked made permanent + request ids made per-instance** | The 30-minute ack window was doing **two** jobs at once: "has this been dispatched?" and "how soon may I rescue this Bot again?". ⇒ Once the window expired, an **already-delivered** request became "undelivered" again ⇒ the delivery side **re-dispatched it every 30 minutes** (≈47k tokens per turn) and interrupted Bots that had already recovered. Now: "dispatched" is **permanent** (keyed by the per-instance request id), the rescue interval is a **separate 30-minute rate limit** owned by the daemon, and every write gets a **fresh instance id** so the two never collide |
| **A roster that cannot be read is now announced** | The old code was `if bots.is_empty() { return }` — **not a single word logged**. "The roster can't be read" was completely invisible ⇒ the daemon looked healthy while **watching nothing**. Now it says so explicitly, once per distinct cause |
| **New wake-up self-check (`bot_bootstrap.rs`)** | After a host restart the Mini App is not started automatically ⇒ "decide" keeps working while "deliver" idles. The daemon now **says so** instead of going quiet, and also detects **delivery spinning** (runtime up, backlog never shrinking) |
| **Runtime-liveness check rewritten** | It used a **hard-coded port (55932)** — the host's default allocation — and that port belongs to the **host process**, not necessarily to this Mini App. Now: a **live process** whose **cwd is this plugin's runtime package** and whose `package.json` **or** `.minimax-plugin/plugin.json` has `name == expiry-guardian` |
| **A documented conclusion corrected** | The docs used to say "waking does not happen while the panel is closed" — **inaccurate**. Measured: the runtime kept running for **44 minutes** with the panel closed and kept delivering. The real gap is a **host restart** |
| **`miniapp publish` fixed** | The manifest's MCP endpoint set did not match the host's in-memory "published snapshot". Declare **exactly one** endpoint (`server=expiry-guardian`, `path=/mcp/expiry-guardian`) + the matching `servers.mcp.json` + `"mcpServers": ["servers.mcp.json"]` in `plugin.json` ⇒ publish succeeds (verified twice) |
| **Several "says something false" branches fixed** | A cleared backlog no longer claims "the runtime recovered"; a *growing* backlog no longer claims "delivery resumed" (only a *shrinking* one does); a failing `lsof` is reported as "the check is broken", not "the runtime is down"; a repeated self-check failure is logged once; surrogate ranges are matched exactly (U+D000–D7FF are ordinary characters, not surrogates) |

| 11 | 🔴 **A typo in the dashboard's data layer broke the whole panel** | `doBuildSnapshot()` wrote `platform` as `platformRes` ⇒ `ReferenceError` ⇒ **`buildSnapshot()` throws outright** ⇒ quota, decision, reset cards and credits all unavailable ⇒ **the panel was broken**. The typo was introduced in **v1.2** (`fd98d98`) and survived four releases. It went unnoticed because the unit tests cover the pure `decide()` function — **the data layer itself was never executed** |
| 12 | 🔴 **The `watchdog_status` MCP tool always returned `{}`** | `deps.status()` is `async`, but the handler never awaited it ⇒ it got a **Promise** ⇒ `JSON.stringify(Promise)` is the string `"{}"`. (`watchdog_wake` and `watchdog_decide` *are* awaited — only `status` was missed.) |
| 13 | ✅ **Two regression tests added** (both bugs survived purely because nothing covered them) | `mcp.test.mjs` (18 checks): MCP protocol, the three tools, **an `async` status must really be awaited** (otherwise it is `"{}"`), and an unknown tool must fail loudly. `server-lint.test.mjs` (5 checks): a static guard for "used but never declared" identifiers — reintroducing the typo makes it fail (5/5 → 3/5) |

Tests: Rust **70/70**, Python **20/20**, MiniApp **136/136**
(bot_wakeup 41 + config-fidelity 15 + decision 58 + mcp 18 + server-lint 5 + retry.behavior 4).

### v1.5 — architecture hardening (2026-10-20, evening)

The architecture is unchanged: the Rust daemon **decides and records**, this
Mini App **delivers** (`session send`). This release is all about hardening the
parts that looked fine but failed silently.

| Change | Effect |
| --- | --- |
| **Wake-session reuse** — the first wake host session id is stored in `bot-wake-host-session.json` and reused | Wake-ups no longer cold-start (≈93k tokens each). Measured on 2026-10-20 02:39: waking 6 Bots created **0** new sessions (it used to create one per Bot) |
| **Request lifetime** — "30-minute TTL" replaced by "at most **3 attempts** (`MAX_ATTEMPTS`) + a **24-hour** backstop (`MAX_REQUEST_AGE_MS`)" | With the panel closed nobody consumed the requests, so after 30 minutes they were all gone — and when the panel reopened the Bots were broken again with **no request left to send** (measured: 7 requests, none consumed, 70 s after closing) |
| **`acked` is no longer a permanent blacklist** — entries carry a timestamp and are suppressed for **30 minutes** only (`ACK_SUPPRESS_WINDOW_MS`) | 6 Bots had broken again (idle 47–94 minutes, far past the 90 s threshold) yet were **never woken again** because their ids sat in `acked`. The old format (plain string array) still reads and is treated as "a long time ago" — no suppression |
| **Three concurrent-write bugs fixed** (temp filename clash / lost update / cleanup condition) | The cleanup-condition bug made the **3-attempt cap an illusion — unlimited retries**, each burning a wake-up turn |
| **Transient errors are not resent** | `BUSY` / `session_busy` (a turn is already in flight) is retried later, **not resent**; `AGENT_OUTCOME_UNKNOWN` (the turn may already have run) is acked outright ⇒ otherwise the Bot receives **two** duplicate "continue from where you stopped" messages |
| **Client 401 self-healing** | The panel no longer sticks on "live connection lost": 3 consecutive 401s ⇒ `window.location.reload()`, capped at **5** reloads (counted in sessionStorage), reset on any success |
| **New local MCP endpoint** | `watchdog_status` / `watchdog_wake` / `watchdog_decide` — other plugins can ask this one directly. Deliberately **no "spend a card"**: spending is a real write (one card fewer) |
| **`50111` is now self-healing** | Short-window rate limiting (HTTP 429) used to fall into "unknown error code ⇒ do not retry", even though 429 is on the Host's retry allowlist |
| **New `check-test-parity.sh`** | The workspace source and the plugin package each keep their own copy of the tests, with different layouts ⇒ changing one and forgetting the other made the package run stale tests |

Tests: Rust **51/51**, Python **20/20**, MiniApp **112/112**
(bot_wakeup 35 + config-fidelity 15 + decision 58 + retry.behavior 4).

The full write-up of each item is in
[`docs/README.zh-CN.md`](docs/README.zh-CN.md); the wake-up channel is broken
down in [`docs/BOT_DIAGNOSIS.md`](docs/BOT_DIAGNOSIS.md).

## Install

1. Install this plugin in MiniMax Code.
2. The Mini App works immediately for the expiry radar — no extra setup.
3. For unattended goal pausing and card spending, install the background daemon:

   ```bash
   # from the plugin directory
   cd daemon/rust
   cargo build --release
   install -m 755 target/release/reset-card-daemon ~/.local/bin/
   ```

   Then register it with `launchd` (see `docs/README.zh-CN.md` for the plist and the
   full command list).

## Configuration

The daemon, the Python reference implementation and the Mini App all read the **same**
config file — nothing hardcodes a threshold in its own code:

```json
{
  "schemaVersion": 1,
  "cardTriggerPercent": 99,
  "cardSkipBeforeResetMinutes": 60,
  "quotaLevels": { "tight": 80, "low": 95 },
  "goalPause": { "enabled": true, "triggerPercent": 95, "resumeAtPercent": 0 }
}
```

| Field | Meaning |
| --- | --- |
| `cardTriggerPercent` | Spend a reset card at this percentage of the 5-hour quota. |
| `cardSkipBeforeResetMinutes` | Do **not** burn a card when the 5-hour window refreshes sooner than this. The quota comes back on its own; spending a card then is pure waste. |
| `quotaLevels.tight` / `.low` | Traffic-light thresholds for the dashboard. |
| `goalPause.enabled` | Master switch for goal auto-pause. |
| `goalPause.triggerPercent` | Pause all in-flight goals at this percentage of the 5-hour quota. |
| `goalPause.resumeAtPercent` | Resume when usage falls back to this percentage. |

Invalid values are **clamped and reported**, never silently corrected — a silent
correction would make you believe a setting took effect when it did not.

The daemon re-reads the file every cycle, so changes take effect within about a minute
without a restart. The Mini App's “Automation thresholds” card edits the same file.

## Bot watchdog: a crashed Bot comes back on its own

MiniMax Bot creates a **new session** every time it fails, so a Bot that keeps
crashing leaves a trail of dead sessions and a new error notice each time.

This package watches your Bot team and re-enables the ones that died from a
**temporary** failure — reusing the Bot's original session instead of creating
another one.

| Situation | What the watchdog does |
| --- | --- |
| HTTP 500, network change, 429/529, TPM rate limit, 400/404/422 | Waits out a backoff, then **re-enables the same session** |
| Token Plan limit reached (42212), credits exhausted (50110), login expired (50112) | **Does not retry.** Tells you exactly what to fix |
| The Bot is fine, or only has a stale error code from an earlier turn | Leaves it alone |

The full diagnosis, with the evidence behind it, is in
[`docs/BOT_DIAGNOSIS.md`](docs/BOT_DIAGNOSIS.md).

### How it re-enables a Bot

The watchdog runs outside the Host (a `launchd` service, no token, unattended),
so it cannot reach MiniMax Bot's MCP endpoint — the Host assigns that port and
its MCP client sends a per-run credential other processes do not have. Four
channels were measured on 2026-10-19 / 2026-10-20:

| Channel | Can it continue another Mini App's session? | Verdict |
| --- | --- | --- |
| `context.agent.run({ sessionId })` | No | `assertSessionOwnership` rejects it: `owner !== caller.pluginId` ⇒ `SESSION_NOT_FOUND`. All 45 Bot sessions carry `purpose = 'miniapp:minimax-bot'` |
| Calling the other Mini App's MCP endpoint directly | No | The Host's MCP client sends a per-run credential that other processes do not have |
| Writing a row into `local_runtime_queue_items` by hand | No | That bypasses `steer`; the Host's queue poller ignores rows it did not write. Measured: still `queued` and unclaimed after 3 minutes, while `session send` was accepted in 5 seconds |
| **Running an Agent that calls the `mavis` tool's `session send`** | **Yes** | `session send` admits on reachability alone — *there is no topology check* — so any session holding the `mavis` tool can wake any Bot session. Measured on 2026-10-20: 7 stuck Bots were all woken and answered |

So the watchdog decides *who* needs waking and writes the request; this Mini App
performs the delivery from inside the Host by starting an agent turn whose agent
calls `session send`. The target Bot keeps its own session, memory and
conversation history.

**Cost.** Each wake costs one agent turn. A *cold* wake — one that creates a
fresh session — measures ≈93k tokens end to end (input 42k / output 374 /
cache read 51k). Since v1.5 the wake-up turn **reuses one host session**
(`bot-wake-host-session.json`), so the system prompt and tool definitions stay
in the context cache and each wake costs only the increment. The loop therefore
runs **only when there is something pending** — it never spins.

**Request lifetime.** A request stays alive until it has been **attempted 3
times** (`MAX_ATTEMPTS`); the 24-hour limit (`MAX_REQUEST_AGE_MS`) is only a
backstop against zombie entries. The attempt counter lives in its own file
(`bot-revive-attempts.json`) because the daemon and this Mini App write
concurrently — a read-modify-write on the shared request file is a lost update.
A successfully woken id is recorded in `bot-revive-acked.json` **with a
timestamp** and is only suppressed for 30 minutes, so a Bot that breaks again
later can be woken again.

## Local MCP endpoint

This Mini App exposes a **local MCP endpoint** so other plugins can ask it
directly from their own turns.

| | |
| --- | --- |
| Implementation | `miniapp/node/mcp.mjs` |
| Declaration | `mcpEndpoints` in `miniapp/miniapp.json` + `servers.mcp.json` at the package root |
| Path | `/mcp/expiry-guardian` (and, for compatibility, `/mcp/minimax-bot`) |
| Transport | streamable-http |

| Tool | What it does |
| --- | --- |
| `watchdog_status` | Current quota, reset-card / credit expiry, and the **pending wake-up queue** (including how many times each request has been attempted) |
| `watchdog_wake` | Run **one wake-up round right now** (does not wait for the 60 s poll) |
| `watchdog_decide` | Decide right now: should a card be spent, which one, and why (**decides only — never spends**) |

⚠️ **"Spend a card" is deliberately not exposed**: spending is a real write
(one card fewer), and letting any plugin spend on a whim is too dangerous ⇒
use the panel button.

⚠️ **A declared endpoint must really exist**: the Host probes it at publish time
with `initialize` + `tools/list` and fails the publish if the probe does not
answer. The Host's in-memory "published snapshot" is also **sticky** — once a
manifest version declares `mcpEndpoints`, later versions must keep declaring the
same endpoints or `miniapp publish` is rejected with `MCP_PUBLICATION_INVALID`.

## Retry policy: retryable failures are retried forever

When the account API answers with a **temporary** failure, the request is retried
until it succeeds — the dashboard does not go blank and the daemon does not treat
a blip as "no data".

| Condition | Behaviour |
| --- | --- |
| **429** (overloaded / rate limited) | Retried forever |
| **500, 502, 503, 504** (internal / gateway) | Retried forever |
| Connection reset, timeout, DNS failure | Retried forever |
| 401 / 403 (session expired) | Token refreshed, then retried once |
| Other 4xx | Fails fast — retrying cannot help |
| Business errors (invalid params, quota exhausted) | Fails fast |

Backoff is exponential with jitter, **capped at 60 s**: the first retry waits about
a second, and no attempt ever waits longer than a minute, so "forever" cannot
turn into "hammering the API". Every attempt is logged, so a stuck request always
says what it is waiting for.

Retries are **interruptible**: `SIGINT` / `SIGTERM` (what `launchctl` sends) make
the in-flight request give up immediately, so the daemon can always be stopped
even while it is waiting on a failing endpoint.

Spending a reset card is retried too, and that is safe: the request carries
`out_biz_no`, which the server treats as an idempotency key, so a repeated
submission replays the result instead of consuming a second card.

## Data integrity: "could not read" is not "there is none"

Reading a balance is not the same as reading a balance of zero. If the credit
endpoints fail, the previous code treated the missing data as "no credits
available" and flipped the decision from *keep the card* to *spend the card* —
and spending a card is a real write.

Now an incomplete read is reported as incomplete, and the round performs **no
write at all** (`wait`). The same rule applies on the daemon side: a failed
platform-credit or Code-credit lookup marks the data incomplete instead of being
logged and ignored.

The dashboard also keeps the raw card list alongside the redacted one it
renders. The redacted list has no `card_id`, so using it to check "is this card
still listed?" was always false — which meant the spend call was skipped every
single time.

## Architecture: who can do what

| Actor | Pause a goal | Start a turn (resume) |
| --- | --- | --- |
| **Daemon** (outside the Host) | Yes — writes the goal store; the runtime sees it at the next turn boundary | No |
| **This Mini App** (inside the Host) | No | Yes — through an agent turn that calls the `mavis` tool's `session send` |
| **Host main process** (Electron IPC) | Yes | Yes — but it listens on no TCP port, so nothing outside can reach it |

Resuming a goal therefore needs a delivery channel that lives inside the Host. The
daemon decides and records; this Mini App performs the delivery. The wake loop runs
**only while there is something to deliver**, because each wake costs an agent turn.

**v1.4 removed the cron watcher.** It was unreliable: the runtime archives the
session a cron watcher runs in, so the next trigger failed with
`CRON_SESSION_TARGET_ARCHIVED` (measured: 4 delivered, 8 failed out of 12), and each
round cost roughly 47k tokens. Nothing about the daemon's decision-making changed —
only the delivery path.

## Configuration files

| Path | Purpose |
| --- | --- |
| `~/.config/mcode-reset-card/config.json` | Shared thresholds (above). |
| `~/.config/mcode-reset-card/goal-pause.json` | Ledger of goals **we** paused, so goals you paused by hand are never touched. |
| `~/.config/mcode-reset-card/goal-resume-requests.json` | Pending wake-up requests. |
| `~/.config/mcode-reset-card/goal-resume-acked.json` | Which goal requests have been handled (idempotency). |
| `~/.config/mcode-reset-card/bot-revive-requests.json` | Pending Bot wake-up requests. |
| `~/.config/mcode-reset-card/bot-revive-acked.json` | Which Bot requests have been handled, **with a timestamp** — suppressed for 30 minutes only. |
| `~/.config/mcode-reset-card/bot-revive-attempts.json` | How many times each request has been attempted (**3 ⇒ void**). Separate file so the daemon and the Mini App never lose each other's writes. |
| `~/.config/mcode-reset-card/bot-wake-host-session.json` | The reused wake-up host session id (saves the cold start). |
| `~/.config/mcode-reset-card/bot-notified.json` | Which Bot errors have already been reported, so the same one is not repeated every minute. |
| `/tmp/mcode-reset-card.log` | Daemon log. |

## Tested environment

- MiniMax Code 3.1.2 or later on macOS (darwin), verified on macOS 27.0 (arm64).
- Rust toolchain with edition 2024 support (the daemon build).
- Node.js 22 or later for the parity checker.
- The daemon talks to the MiniMax account endpoints directly and reads the local
  MiniMax Code state store. It needs the desktop app to be running at least once to
  obtain an OAuth lease, but it keeps working after the app is closed.

## Data & access

- **Files read:** `~/.config/mcode-reset-card/*.json` (its own config and ledgers),
  `~/.minimax/v2/sqlite/runtime-state.sqlite` (read and, for goal status, written).
- **Files written:** the config and ledger files above; `~/.local/bin/reset-card-daemon`
  if you install it there.
- **Network:** `www.minimaxi.com` (quota, reset cards, credit expiry) and
  `agent.minimaxi.com` (Code credit expiry). No other hosts.
- **Credentials:** the daemon reads the OAuth lease issued by the local desktop app.
  It never stores a token itself, and this repository contains no credentials.
- **Agent tasks:** the Mini App runs a small wake loop that reads the request files and
  calls `session send` on your behalf to wake a paused goal or a stuck Bot. It runs
  only when something is pending, does no other task, and changes nothing except
  those files and the Bot's own session.

## Known boundaries

- Personal-plan quotas only; team-plan endpoints differ.
- The dashboard lives in the Mini App, so closing the panel stops the board. The
  daemon is unaffected.
- A goal whose session has been idle for a long time, or one stuck inside an
  unfinished turn, cannot be woken from outside — open that session in MiniMax Code
  once so the runtime has a live session to attach to.
- **The Mini App has no resident mode.** The Host accepts `lifecycle: "background"`
  in the manifest but forces it back to `"on-demand"` when it reads it. Measured
  (2026-10-20): the runtime kept running for **44 minutes** with the panel closed
  and kept delivering — the idle drain only fires after a lease is released, and a
  lease is only held while a turn is running. The real gap is a **host restart**:
  the Host does not start any Mini App automatically afterwards. The daemon now
  detects that case and says so explicitly instead of going quiet.
- Each wake costs one agent turn (≈93k tokens when cold; an increment once the
  host session is reused), and a Bot that fails with a rate-limit error (50150)
  may need another attempt on a later round.
- **The official `minimax-bot` plugin cannot be forked locally** — `miniapp publish`
  returns `OFFICIAL_PLUGIN_READ_ONLY`. The wake-up side therefore has to be this
  Mini App (it holds `agentAccess: ["session.run"]`).
- ⚠️ **Known Host-side issue**: once a manifest version declares `mcpEndpoints`,
  later versions must keep declaring the same endpoints, otherwise
  `miniapp publish` is rejected with `MCP_PUBLICATION_INVALID` (the Host's
  "published snapshot" in memory is sticky). The current workspace deploys by
  syncing straight into `~/.minimax/plugins/expiry-guardian/` and running
  `miniapp open`.

## Documentation

Full architecture notes, the twenty pitfalls this package works around, and the
verification recipes are in [`docs/README.zh-CN.md`](docs/README.zh-CN.md).
The wake-up channel — every measured dead end included — is in
[`docs/BOT_DIAGNOSIS.md`](docs/BOT_DIAGNOSIS.md).

## License

MIT. See [LICENSE](LICENSE).
