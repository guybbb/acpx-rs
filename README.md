# acpx-rs

> Fast, reliable ACP session orchestration in Rust.

`acpx-rs` keeps ACP agents warm behind a small CLI so you can create a named session once, prompt it repeatedly, and avoid paying startup cost on every turn.

It is built for local workflows that want three things:

- low-latency follow-up prompts
- durable session state on disk
- simple recovery when a socket or worker goes stale

## Why it exists

Most ACP-driven tools are good at talking to an agent once. `acpx-rs` is built for talking to the same agent over and over without rebuilding the world each time.

It starts a background owner process per named session, keeps the ACP session ID around, streams output back to the caller, and persists enough metadata to make the whole loop dependable.

## Features

- Named persistent sessions
- Live streamed prompt output
- On-disk history and last assistant reply
- Session status inspection as JSON
- Automatic stale socket cleanup on restart
- Per-session logs
- Optional `model` and `mode` configuration at session startup

## Quick Start

Build it:

```bash
cargo build --release
```

Create or reuse a session:

```bash
./target/release/acpx sessions ensure \
  --name demo \
  --agent "your-agent --acp" \
  --cwd .
```

Send a prompt:

```bash
./target/release/acpx prompt --session demo "Explain what this repository does."
```

Inspect the session:

```bash
./target/release/acpx status --session demo
```

Get the last assistant reply again:

```bash
./target/release/acpx sessions last demo
```

Close the session cleanly:

```bash
./target/release/acpx sessions close demo
```

If you prefer installing locally instead of calling the binary from `target`, use:

```bash
cargo install --path .
```

## Command Surface

```text
acpx prompt --session <SESSION> [--json] [--file <PATH>] <TEXT...>
acpx status --session <SESSION>
acpx sessions ensure --name <NAME> --agent <COMMAND> [--cwd <DIR>] [--startup-timeout <SECS>] [--model <MODEL>] [--mode <MODE>]
acpx sessions last <NAME>
acpx sessions close <NAME>
acpx sessions list [--active]
acpx sessions cleanup [--max-session-age-days N] [--max-log-age-days N] [--max-log-size-mb N] [--force]
```

The `--file` flag reads the prompt from a file (`--file -` for stdin), useful for long or multi-line prompts that are awkward as shell args.

## How It Works

1. `sessions ensure` writes a session record and spawns a background owner process.
2. The owner process starts your ACP-capable agent command and creates a fresh ACP session.
3. `prompt` connects over a Unix socket, streams chunks back to your terminal, and saves the final assistant reply to disk.
4. `status`, `sessions last`, and `sessions close` operate on the same named session record.

If an old daemon died but left a stale socket behind, `acpx-rs` detects that and recreates the session instead of hanging on bad state.

## Storage

By default, state lives under `~/.acpx-rs`:

```text
~/.acpx-rs/
  sessions/   # session metadata and message history
  sockets/    # unix sockets for owner processes
  logs/       # per-session daemon logs
```

You can override the root directory with `--home`.

## What Makes It Fast

The speedup comes from reusing a live ACP session instead of re-spawning and re-initializing the agent for every prompt. For iterative coding, debugging, and operator-style workflows, that usually matters more than shaving a few milliseconds off the CLI itself.

## OpenClaw Integration

acpx-rs ships with an OpenClaw plugin (in `extension/`) that registers it as an ACP runtime backend. This lets the OpenClaw agent dispatch tasks to ACP agents (Codex, Gemini, Claude Code) through the gateway.

The plugin includes a watchdog that polls session status every 30s when the event stream is quiet. If the session dies (agent crash, capacity error, etc.), the watchdog detects it and reports the actual error back to OpenClaw instead of hanging.

### Layout

```text
acpx-rs/
  src/main.rs              # Rust CLI + daemon
  extension/               # OpenClaw plugin (TypeScript)
    index.ts               # Plugin entry point
    openclaw.plugin.json   # Plugin manifest + config schema
    package.json
    src/
      runtime.ts           # AcpRuntime implementation (spawns CLI, streams events)
      service.ts           # Plugin service registration
```

When deployed, the plugin lives in the openclaw user's global extensions dir so it survives `openclaw update`:

| Component | Deploy location |
|-----------|----------------|
| Binary | `/usr/local/bin/acpx-rs` |
| Plugin | `~openclaw/.openclaw/extensions/acpx-rs/` |

### Deploy

Build the binary and copy the plugin:

```bash
cd ~/repos/acpx-rs
cargo build --release
sudo cp target/release/acpx /usr/local/bin/acpx-rs
```

Deploy the plugin (run after cloning, or after any TypeScript change):

```bash
sudo cp -r extension/* /home/openclaw/.openclaw/extensions/acpx-rs/
sudo chown -R openclaw:openclaw /home/openclaw/.openclaw/extensions/acpx-rs/
```

Do **not** put the plugin in `node_modules` — it gets wiped on every `openclaw update`.

### Configuration

```bash
# Binary path
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.command /usr/local/bin/acpx-rs

# Default model and startup timeout
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.defaultModel "gpt-5.4/high"
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.startupTimeout 60

# Agent mappings
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.agents.codex.command "npx -y @zed-industries/codex-acp"
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.agents.codex.model "gpt-5.4/high"
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.agents.codex.mode "auto"
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.agents.claude.command "npx -y @zed-industries/claude-agent-acp"
sudo -u openclaw openclaw config set plugins.entries.acpx-rs.config.agents.gemini.command "gemini --experimental-acp --yolo -m gemini-3.1-pro-preview"

# Set as default ACP backend
sudo -u openclaw openclaw config set acp.backend acpx-rs
```

Restart the gateway to apply:

```bash
sudo systemctl restart openclaw-gateway.service
```

### Agent compatibility notes

| Agent | set_config_option | set_mode | Notes |
|-------|-------------------|----------|-------|
| Codex (`codex-acp`) | Yes | Yes | Model/mode applied via ACP protocol after session init |
| Claude (`claude-agent-acp`) | Yes | Yes | Strips `CLAUDECODE` env var automatically |
| Gemini (`gemini --experimental-acp`) | No | Partial (only `default`/`autoEdit`/`yolo`) | Pass `-m <model>` in the command string; `set_config_option` and unsupported modes fail gracefully with a warning |

Key: Gemini's ACP adapter uses `--experimental-acp` (not `--acp`), and does not support `session/set_config_option`. The model must be baked into the agent command via `-m`. The `set_config_option` and `set_mode` failures are non-fatal — the session continues.

### Resilience

- **Dead agent detection**: The daemon polls with `recv_timeout(5s)` + `try_wait()` to detect crashed agents, even when child processes keep stdout open.
- **Write-error detection**: The streaming callback tracks broken-pipe errors. If the client disconnects mid-turn, the daemon bails with a clear error instead of silently losing the Done event.
- **Error preservation**: Agent errors (e.g. "No capacity available for model") are saved to session history and `death_reason` so they are visible in `acpx status` and session records.
- **Stderr capture**: Agent stderr is logged to the session log as `[agent:stderr]` lines and the last 50 lines are included in error messages for crash diagnostics. The TS runtime also captures CLI stderr for the same reason.
- **Watchdog (TS runtime)**: If no events arrive for 30s, the plugin polls `acpx status` to check if the session died. If closed/unreachable, it kills the child and reports the actual `death_reason` instead of hanging.
- **Auto-exit on death**: When the agent dies during a prompt, the daemon marks the session closed and exits. The next `sessions ensure` recreates everything from scratch.
- **Non-fatal config**: `set_config_option` and `set_mode` failures are warnings, not fatal — agents that don't support them (e.g. Gemini) still work.

### Verify

Check gateway logs after restart:

```bash
sudo journalctl -u openclaw-gateway.service --since "1 min ago" | grep "acpx-rs"
# Expect:
#   acpx-rs runtime backend registered (command: /usr/local/bin/acpx-rs)
#   acpx-rs runtime backend ready
```

Test directly:

```bash
sudo -u openclaw /usr/local/bin/acpx-rs sessions ensure \
  --name test --agent "npx -y @zed-industries/codex-acp" \
  --model "gpt-5.4/high" --mode "auto" --cwd /tmp --startup-timeout 60

sudo -u openclaw /usr/local/bin/acpx-rs prompt -s test "Say hello"
sudo -u openclaw /usr/local/bin/acpx-rs sessions close test
```

## Project Status

Early, focused, and usable. The current implementation is intentionally small: one binary, one job, minimal ceremony.

If you want a lightweight ACP broker that is easy to inspect, script, and extend, this is the point of the project.
