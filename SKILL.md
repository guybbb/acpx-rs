---
name: acpx-rs
description: Drive ACP harness sessions (Codex, Claude Code, Gemini, Pi) via the acpx-rs Rust broker. Preferred over bundled acpx for all harness routing — faster startup, proper ACP protocol for model/mode, and agent-agnostic design.
user-invocable: false
---

# acpx-rs — Rust ACP Session Broker

**Prefer `acpx-rs` over bundled `acpx` for all ACP harness routing.** The Rust broker is faster, handles model/mode via ACP protocol (not CLI flags), and works with any ACP-compatible agent.

## When to use this skill

- Any time the `acp-router` skill would be triggered (user asks to run something in Codex/Claude/Gemini/Pi)
- Read this skill **alongside** `acp-router` to get the correct binary and command templates
- This skill **overrides** the `acp-router` "Direct acpx path" section

## Two execution paths

1. **Direct CLI** (via `exec` tool + this skill's binary): use the templates below
2. **ACP runtime** (via `sessions_spawn`): the plugin handles routing automatically; configured as the `acpx-rs` backend

## Binary

Co-located in this skill directory:
```
~/.openclaw/workspace/skills/acpx-rs/acpx-rs
```

Set and reuse:
```bash
ACPX_CMD="~/.openclaw/workspace/skills/acpx-rs/acpx-rs"
```

Also available system-wide at `/usr/local/bin/acpx-rs`.

## Key differences from bundled acpx

| Feature | acpx (Node.js) | acpx-rs (Rust) |
|---------|----------------|----------------|
| Agent selection | Per-agent subcommands (`acpx codex ...`) | `--agent <command>` flag |
| Model/mode | CLI flags on agent | ACP protocol after session init |
| Session create | `sessions new --name` | `sessions ensure --name` (idempotent) |
| Prompt | `acpx codex -s name "text"` | `acpx-rs prompt -s name "text"` |

## Agent command mapping

When user names a harness, map to these `--agent` values:

- "codex" → `--agent "npx -y @zed-industries/codex-acp"`
- "claude" or "claude code" → `--agent "npx -y @zed-industries/claude-agent-acp"`
- "gemini" or "gemini cli" → `--agent "gemini --experimental-acp --yolo"`
- "pi" → `--agent "pi --acp"`

## Model and mode

Pass `--model` and `--mode` on `sessions ensure`. The broker applies them via ACP protocol after session initialization.

Composite model IDs (e.g. `gpt-5.4/high`) are automatically split:
- `gpt-5.4/high` → model=`gpt-5.4`, reasoning_effort=`high`
- `gemini-2.5-flash` → model=`gemini-2.5-flash` (no split)

Default for Codex: `--model "gpt-5.4/high"` `--mode "auto"`

## Command templates

### Create or reconnect session (idempotent)

```bash
${ACPX_CMD} sessions ensure \
  --name oc-<agent>-<conversationId> \
  --agent "<agent-command>" \
  --model "<model>" \
  --mode "<mode>" \
  --cwd "<working-dir>" \
  --startup-timeout 30
```

### Send prompt (JSON streaming — use for programmatic consumption)

```bash
${ACPX_CMD} prompt -s oc-<agent>-<conversationId> --json "<prompt text>"
```

### Send prompt (plain text — use for relay to user)

```bash
${ACPX_CMD} prompt -s oc-<agent>-<conversationId> "<prompt text>"
```

### Check session status

```bash
${ACPX_CMD} status -s oc-<agent>-<conversationId>
```

### Show last assistant response

```bash
${ACPX_CMD} sessions last oc-<agent>-<conversationId>
```

### Close session

```bash
${ACPX_CMD} sessions close oc-<agent>-<conversationId>
```

## Session naming

Use `oc-<agent>-<conversationId>` where:
- `<agent>` = codex, claude, gemini, pi, etc.
- `<conversationId>` = thread id when available, otherwise channel/conversation id

## Working with ACP sessions — autonomous flow

When you spawn an ACP agent session to perform a task, follow this flow:

1. **Create the session** with `sessions ensure`
2. **Send the task prompt** — include all context the agent needs (cwd, files, goals, constraints)
3. **Read the streamed response** — the `prompt` command streams the agent's full response including tool use and reasoning
4. **Act on the result** — parse the agent's response, extract what was done, relay a summary to the user
5. **Do NOT wait for user input** between steps 2–4. Complete the full cycle autonomously.
6. **Report back** to the user only when the task is completed (or failed), with a concise summary of what was accomplished.

### Key rules

- **Never block on user input** while an ACP session is running. The agent works independently — let it finish, then report results.
- **The prompt response IS the result.** The `prompt` command returns the agent's full response. Read it, summarize it, and relay to the user.
- **Handle errors gracefully.** If the session dies (broken pipe, agent crash), report the failure reason from the error message and offer to retry.
- **Close sessions when done.** After the task is complete, close the session with `sessions close` to free resources.
- **Session auto-recovery.** If a session is closed or dead, `sessions ensure` recreates it from scratch. You can always retry.

### Example autonomous flow

```bash
ACPX_CMD="~/.openclaw/workspace/skills/acpx-rs/acpx-rs"

# 1. Create session
${ACPX_CMD} sessions ensure --name oc-codex-123 \
  --agent "npx -y @zed-industries/codex-acp" \
  --model "gpt-5.4/high" --mode "auto" \
  --cwd /path/to/repo --startup-timeout 60

# 2. Send task and capture response
RESPONSE=$(${ACPX_CMD} prompt -s oc-codex-123 "Fix the failing test in src/utils.test.ts and commit the fix")

# 3. RESPONSE now contains the agent's full output — summarize and relay to user
# Do NOT ask the user what to do next. Just report what happened.

# 4. Clean up
${ACPX_CMD} sessions close oc-codex-123
```

## Complete example — Codex via Telegram

```bash
ACPX_CMD="~/.openclaw/workspace/skills/acpx-rs/acpx-rs"

# Create session
${ACPX_CMD} sessions ensure \
  --name oc-codex-361509501 \
  --agent "npx -y @zed-industries/codex-acp" \
  --model "gpt-5.4/high" \
  --mode "auto" \
  --cwd /tmp

# Send prompt and get response
${ACPX_CMD} prompt -s oc-codex-361509501 "Write a Python hello world"

# Get last response (for relay)
${ACPX_CMD} sessions last oc-codex-361509501
```

## Troubleshooting

- **Session not starting**: Check `--startup-timeout` (default 30s); some agents need longer
- **Model not applied**: Ensure the agent's ACP adapter supports `session/set_config_option`
- **CLAUDECODE error**: The broker automatically strips `CLAUDECODE` env var for Claude sessions
- **Permission errors on cwd**: The broker skips `current_dir` if the path is inaccessible
- **Broken pipe / agent crash**: The daemon auto-exits and marks the session closed. Run `sessions ensure` again to recreate. Check session logs at `~/.acpx-rs/logs/<name>.log` for `[error]` and `[agent:stderr]` entries.
