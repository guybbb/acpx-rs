---
name: acpx-rs
description: ACP session broker. Dispatch work to Codex, Claude Code, Gemini, or the investment agent. Call directly via exec — results return in your context.
user-invocable: false
---

# acpx-rs — ACP Session Broker

Manages long-running coding agent sessions. You call it directly via exec — no subagent needed.

## How to dispatch work

Two sequential exec calls. Use ONLY `command` and `timeout` parameters — no other exec parameters.

### Step 1 — Ensure session exists

```
exec(command: "~/.openclaw/workspace/skills/acpx-rs/acpx-rs sessions ensure --name <session-name> --agent \"<agent-command>\" --cwd /home/openclaw --startup-timeout 60 --quiet", timeout: 120)
```

### Step 2 — Run the prompt (blocks 1–10 min, this is normal — WAIT for it)

```
exec(command: "~/.openclaw/workspace/skills/acpx-rs/acpx-rs prompt -s <session-name> --summarize \"<user's task>\"", timeout: 600)
```

### Step 3 — Deliver the output from step 2 to the user. Include the session name.

## Agent commands and session names

| Agent | Session prefix | Command |
|-------|---------------|---------|
| Gemini | `oc-gemini-` | `gemini --experimental-acp --yolo -m gemini-3-flash-preview` |
| Codex | `oc-codex-` | `npx -y @zed-industries/codex-acp` |
| Claude | `oc-claude-` | `npx -y @zed-industries/claude-agent-acp` |
| Investment | `oc-invest-` | `/home/openclaw/.openclaw/workspace-trading/trading-app/.venv/bin/python /home/openclaw/.openclaw/workspace-trading/trading-app/run.py --investment-agent-acp` |

Use the conversation/thread ID as suffix (e.g., `oc-gemini-1346`).

## Important rules

- Do NOT use `sessions_spawn` or subagents — call exec directly.
- Do NOT add `yieldMs`, `background`, `security`, `host`, `ask`, or `pty` to exec calls.
- The prompt command blocks while the coding agent works. This is expected. Wait for it.
- Always include `--cwd /home/openclaw` in the ensure command.

## Diagnostic commands (exec)

```bash
ACPX=~/.openclaw/workspace/skills/acpx-rs/acpx-rs
${ACPX} status -s <session-name>
${ACPX} sessions list --active
${ACPX} sessions last <session-name>
${ACPX} sessions close <session-name>
```

## Session lifecycle

- `sessions ensure` is idempotent — reuses alive sessions, recreates dead ones
- Sessions persist across tasks (agent retains context)
- Idle timeout: 30 minutes
- Do NOT close sessions after each task
