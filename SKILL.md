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
- "investment" or "trading agent" → `--agent "/home/openclaw/.openclaw/workspace-trading/trading-app/.venv/bin/python /home/openclaw/.openclaw/workspace-trading/trading-app/run.py --investment-agent-acp"`
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

## Session lifecycle

### How `sessions ensure` works

`sessions ensure` is **idempotent** — it is safe to call before every prompt:
- If the session **already exists and is alive**, it returns immediately (no-op). The existing agent keeps its full context and history.
- If the session **is dead or closed**, it recreates everything from scratch (new agent, new ACP session).
- If the session **does not exist**, it creates a new one.

**Always call `sessions ensure` before sending a prompt.** This is your only entry point — it handles both first-time creation and recovery.

### Session reuse — keep sessions alive

Sessions are **tied to a project/workspace**, not to a single task. A warm session retains the agent's full context (loaded files, conversation history, workspace understanding), which makes follow-up tasks faster and more accurate.

**Rules for session reuse:**
- **Reuse sessions across related tasks in the same project.** If the user asks to fix a bug and then asks to add a test in the same repo, reuse the same session — the agent already knows the codebase.
- **Do NOT close a session immediately after one task.** Keep it alive in case more tasks come for the same project.
- **Close a session only when:** the user explicitly says they're done, the conversation ends, or you're switching to a completely different project/workspace.
- **One session per agent per project.** Use the naming convention `oc-<agent>-<conversationId>` to naturally scope sessions to conversations.

### Auto-recovery

If a session dies (agent crash, broken pipe, timeout), the daemon automatically marks it as closed and exits. On the next call:
1. `sessions ensure` detects the closed/dead state
2. It recreates the session from scratch (new agent process, fresh ACP session)
3. The new session is ready to accept prompts

**You do not need to manually detect or handle crashes.** Just always call `sessions ensure` before `prompt` — if the session died, it gets rebuilt transparently. If the prompt itself fails, report the error to the user and offer to retry (the next `sessions ensure` + `prompt` will use a fresh session).

## Session cleanup and lifecycle management

### Idle timeout

Daemon processes automatically self-terminate after **30 minutes of inactivity** (no incoming connections). When the idle timeout fires, the daemon marks the session as closed and removes the socket. The next `sessions ensure` call will create a fresh session.

Daemons also detect when their agent process dies while idle and exit immediately.

### `sessions cleanup` — reap dead sessions and prune old data

Run periodically (or manually) to clean up accumulated session data:

```bash
# Dry-run (show what would be cleaned, no changes)
${ACPX_CMD} sessions cleanup

# Actually clean up
${ACPX_CMD} sessions cleanup --force
```

What it does:
1. **Reaps dead daemons**: finds sessions where the daemon PID is dead but not marked closed, marks them closed, removes stale sockets
2. **Prunes old session records**: deletes closed session JSON files older than 14 days (configurable via `--max-session-age-days`)
3. **Truncates oversized logs**: logs larger than 500MB are truncated to keep the last 10MB (configurable via `--max-log-size-mb`)
4. **Deletes old logs**: log files older than 7 days for closed/missing sessions are removed (configurable via `--max-log-age-days`)
5. **Removes orphan sockets**: sockets with no matching session record or a closed session

### `sessions list` — view all sessions

```bash
# List all sessions (active and closed)
${ACPX_CMD} sessions list

# List only active sessions
${ACPX_CMD} sessions list --active
```

## Working with ACP sessions — autonomous flow

When you spawn an ACP agent session to perform a task:

1. **`sessions ensure`** — call before every prompt (creates or reuses)
2. **`prompt`** — send the task with all needed context
3. **Read the response** — the prompt command streams the full agent output
4. **Report to user** — summarize what the agent did. Do NOT wait for user input between steps 2–4.
5. **Keep the session alive** — do NOT close it unless the conversation is ending or the user is switching projects

### Completion reporting — always confirm when done

When dispatching work to an ACP agent, **always include a completion instruction** in your prompt so you (and the user) know the job finished. Append something like:

> When you are done, end your response with a clear summary of what you did and confirm that the task is complete.

This is critical because:
- Without it, the agent may finish silently and you won't know whether the task succeeded, failed, or is still running
- The user needs a clear signal that the dispatched work is done
- It helps you decide whether to relay success or investigate further

After receiving the `done` event from the prompt, **always report the outcome back to the user** — even if the agent's response is empty or terse. A simple "Codex finished: [summary]" is better than silence.

### Key rules

- **Never block on user input** while an ACP session is running. Let the agent finish, then report results.
- **The prompt response IS the result.** Read it, summarize it, and relay to the user.
- **Handle errors gracefully.** If a prompt fails, report the error and offer to retry. The next `sessions ensure` auto-recovers.
- **Reuse sessions.** Do not close after every task. The agent retains context across prompts — this is the main value of persistent sessions.
- **Always confirm completion.** After the agent finishes, tell the user what happened. Never leave the user wondering if a dispatched job is still running.

### Example: multi-task reuse

```bash
ACPX_CMD="~/.openclaw/workspace/skills/acpx-rs/acpx-rs"

# First task — session is created
${ACPX_CMD} sessions ensure --name oc-codex-123 \
  --agent "npx -y @zed-industries/codex-acp" \
  --model "gpt-5.4/high" --mode "auto" \
  --cwd /path/to/repo --startup-timeout 60
${ACPX_CMD} prompt -s oc-codex-123 "Fix the failing test in src/utils.test.ts"
# → report result to user, keep session alive

# Second task — session is reused (agent already knows the codebase)
${ACPX_CMD} sessions ensure --name oc-codex-123 ...  # no-op, session alive
${ACPX_CMD} prompt -s oc-codex-123 "Now add a test for the edge case with empty input"
# → report result to user, keep session alive

# Conversation ends — clean up
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
