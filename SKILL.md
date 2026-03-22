---
name: acpx-rs
description: ACP session broker backend. Use sessions_spawn (ACP runtime) to dispatch work to Codex, Claude Code, Gemini, Pi. Do NOT use exec for prompts — use sessions_spawn for deterministic delivery.
user-invocable: false
---

# acpx-rs — Rust ACP Session Broker

## How to use — ALWAYS use `sessions_spawn`

**Do NOT use `exec` to run `acpx-rs prompt`.** Use `sessions_spawn` instead. The ACP runtime delivers results deterministically through the projector. The exec path is unreliable — the LLM may silently drop results and the user sees nothing.

To dispatch work to a coding agent:

```json
{
  "task": "<the work to perform>",
  "runtime": "acp",
  "agentId": "<agent>",
  "thread": true,
  "mode": "session"
}
```

**Always tell the user the session name** right after spawning so they can track it.

## AgentId mapping

- "codex" → `agentId: "codex"`
- "claude" or "claude code" → `agentId: "claude"`
- "gemini" or "gemini cli" → `agentId: "gemini"`
- "pi" → `agentId: "pi"`
- "investment" or "trading agent" → `agentId: "investment"`

## Model and mode defaults

The acpx-rs plugin config defines defaults per agent. Do NOT pass model/mode overrides in `sessions_spawn` unless the user explicitly requests a specific model.

- **Codex**: plugin config sets `gpt-5.4-codex/high`, mode `auto`
- **Gemini**: no model/mode override — Gemini auto-selects (`auto-gemini-3`) and runs in `yolo` mode
- **Claude**: no model/mode override — Claude Code uses its own defaults

## Completion and error reporting

The ACP runtime delivers results automatically. After calling `sessions_spawn`:

1. The runtime creates/reuses the acpx-rs session
2. Sends the prompt to the agent
3. Streams events (status updates, text, done/error) back through the projector
4. The projector delivers to the user's channel (Telegram, web, etc.)

**You do not need to relay the response yourself.** The delivery is deterministic.

If the spawn fails, report the error to the user immediately.

## Diagnostic commands (exec only — never for prompts)

Use `exec` only for status checks, not for sending prompts:

```bash
ACPX_CMD="~/.openclaw/workspace/skills/acpx-rs/acpx-rs"

# Check session status
${ACPX_CMD} status -s <session-name>

# List active sessions
${ACPX_CMD} sessions list --active

# Get last assistant response
${ACPX_CMD} sessions last <session-name>

# Close a session
${ACPX_CMD} sessions close <session-name>
```

## Session naming

Sessions are named `oc-<agent>-<conversationId>` where:
- `<agent>` = codex, claude, gemini, pi, etc.
- `<conversationId>` = thread id when available, otherwise channel/conversation id

## Session lifecycle

- **`sessions ensure` is idempotent** — safe to call repeatedly. Reuses alive sessions, recreates dead ones.
- **Sessions persist across tasks** in the same project. The agent retains context.
- **Auto-recovery**: if a session dies, the next `sessions ensure` rebuilds it transparently.
- **Idle timeout**: daemons self-terminate after 30 minutes of inactivity.
- **Do NOT close sessions** after each task. Keep them alive for follow-up work.

## Troubleshooting

- **Session not starting**: Check startup timeout (default 60s in plugin config)
- **Model not applied**: Some agents (Gemini) don't support `set_config_option` — this is handled gracefully
- **CLAUDECODE error**: The broker strips `CLAUDECODE` env var automatically
- **Session died**: Run `sessions ensure` again — it auto-recovers
- **Check logs**: `~/.acpx-rs/logs/<session-name>.log` for `[error]` and `[agent:stderr]`
