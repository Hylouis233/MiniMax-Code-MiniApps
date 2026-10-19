# Bot 看门狗 · Bot Watchdog

MiniMax Bot 会话一遇到错误就会**新建一个会话**继续，于是表现为「bot 反复挂掉」：
每挂一次多一个会话，界面上又是一条错误通知。

The watchdog watches your Bot team and re-enables a Bot that died from a **temporary**
failure — reusing its original session instead of creating a new one. Failures that
retrying cannot fix (quota exhausted, expired login) are reported plainly instead of
burning tokens on pointless retries.

## What it does

| Situation | What the watchdog does |
| --- | --- |
| Bot died from HTTP 500, network change, 429 or 529 | Waits out a backoff, then **re-enables the same session** and reports what it did |
| Bot hit the Token Plan limit (42212), ran out of credits (50110), or the login expired (50112) | **Does not retry.** Tells you exactly what to fix |
| Bot is fine, or merely has a stale error code from an earlier turn | Leaves it alone |

## Why the errors are classified

The two kinds of failure look identical from the outside but need opposite handling:

- **Temporary** — the server hiccupped or the network changed. Retrying works.
- **Deterministic** — the Token Plan usage limit is reached. Retrying never works;
  only topping up the plan or enabling automatic credit consumption does.

Treating the second as the first is what makes a Bot look like it keeps crashing.

The full diagnosis, with the evidence behind it, is in
[`docs/DIAGNOSIS.md`](docs/DIAGNOSIS.md).

## How it re-enables a Bot

Three channels were measured on 2026-10-19:

| Channel | Can it continue another Mini App's session? | Verdict |
| --- | --- | --- |
| `context.agent.run({ sessionId })` | No | The contract says *each Mini App owns its sessions and can continue only those* |
| Calling the other Mini App's MCP endpoint directly | No | The Host's MCP client sends a per-run credential that other processes do not have |
| **Running an Agent that calls the `message_bot` MCP tool** | **Yes** | Measured: a Bot stuck on 42212 woke up and answered within 13 seconds |

So the watchdog declares `mcpServers` pointing at `minimax-bot`, runs a short Agent
turn, and lets that Agent deliver the wake-up message through `message_bot`. The
target Bot keeps its own session, memory and conversation history.

## Configuration

The watchdog reads the Bot roster from MiniMax Bot's own `state.json` and the session
status from the runtime state store. It writes only its own backoff bookkeeping.

| Path | Purpose |
| --- | --- |
| `~/.minimax/v2/plugin-data/liveboards/minimax-bot/state.json` | Read-only: the Bot roster and the Bot → session map. |
| `~/.minimax/v2/sqlite/runtime-state.sqlite` | Read-only: session status, error code, last activity. |
| `~/.minimax/v2/plugin-data/liveboards/bot-watchdog/state.json` | The watchdog's own backoff bookkeeping. |

## Safety notes

- It never creates a session. It only continues existing ones.
- It never retries a quota or authentication failure.
- It reads a session's **last activity** before acting, because a session's
  `error_code` can be a leftover from an earlier turn — the runtime does not clear
  it. Acting on a stale code would interrupt a Bot that is happily working.
- Backoff is exponential with jitter and capped at 60 seconds, so a failing Bot is
  never hammered.

## Tested environment

- MiniMax Code 3.1.2 or later on macOS (darwin), verified on macOS 27.0 (arm64).
- Requires the `minimax-bot` plugin to be installed: the watchdog uses its
  `message_bot` tool to wake a Bot.
- Node.js 22 or later for the tests.

## Data & access

- **Files read:** MiniMax Bot's `state.json` and the runtime state store (read-only).
  The watchdog's own `state.json` is written under the `bot-watchdog` liveboard
  directory.
- **Network:** the watchdog itself talks to no external host. The MCP connection to
  `minimax-bot` is a loopback call inside MiniMax Code.
- **Agent turns:** the watchdog runs one short Agent turn per wake-up. That turn is
  instructed to do exactly one thing — deliver the wake-up message through
  `message_bot` — and nothing else.
- **Credentials:** none are read, stored, or forwarded by the watchdog.

## License

MIT. See [LICENSE](LICENSE).
