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
its MCP client sends a per-run credential other processes do not have. Three
channels were measured on 2026-10-19:

| Channel | Can it continue another Mini App's session? | Verdict |
| --- | --- | --- |
| `context.agent.run({ sessionId })` | No | The contract says *each Mini App owns its sessions and can continue only those* |
| Calling the other Mini App's MCP endpoint directly | No | The Host's MCP client sends a per-run credential that other processes do not have |
| **Running an Agent that calls the `message_bot` MCP tool** | **Yes** | Measured: a Bot stuck on 42212 woke up and answered within 13 seconds |

So the watchdog decides *who* needs waking and writes the request; a small cron
watcher in the main agent directory performs the delivery through `message_bot`.
The target Bot keeps its own session, memory and conversation history.

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
| **Mini App** (inside the Host) | No | No — `context.agent.run` only reaches sessions in its own agent directory |
| **Cron watcher** (main agent directory) | Yes | Yes — `session send` reaches any session |
| **Host main process** (Electron IPC) | Yes | Yes — but it listens on no TCP port, so nothing outside can reach it |

Resuming a goal therefore needs a delivery channel that lives in the main agent
directory. The daemon decides and records; a small cron watcher performs the delivery.
The cron is armed **only while there is something to deliver**, because each of its
rounds costs roughly 47k tokens.

## Configuration files

| Path | Purpose |
| --- | --- |
| `~/.config/mcode-reset-card/config.json` | Shared thresholds (above). |
| `~/.config/mcode-reset-card/goal-pause.json` | Ledger of goals **we** paused, so goals you paused by hand are never touched. |
| `~/.config/mcode-reset-card/goal-resume-requests.json` | Pending wake-up requests. |
| `~/.config/mcode-reset-card/goal-resume-acked.json` | Which requests have been handled (idempotency). |
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
- **Agent tasks:** the Mini App runs a small watcher cron that reads two JSON files and
  calls `session send` on your behalf to wake a paused goal. It does not run any other
  task, and it changes nothing except those two files.

## Known boundaries

- Personal-plan quotas only; team-plan endpoints differ.
- The dashboard lives in the Mini App, so closing the panel stops the board. The
  daemon is unaffected.
- A goal whose session has been idle for a long time, or one stuck inside an
  unfinished turn, cannot be woken from outside — open that session in MiniMax Code
  once so the runtime has a live session to attach to.

## Documentation

Full architecture notes, the eleven pitfalls this package works around, and the
verification recipes are in [`docs/README.zh-CN.md`](docs/README.zh-CN.md).

## License

MIT. See [LICENSE](LICENSE).
