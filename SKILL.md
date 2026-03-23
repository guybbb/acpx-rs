---
name: acpx-rs
description: ACP session broker. Dispatch work to Codex, Claude Code, or Gemini via sessions_spawn subagent. Results auto-announce back into your context.
user-invocable: false
---

# acpx-rs — ACP Session Broker

## How to dispatch work

Use `sessions_spawn` with `runtime="subagent"` to dispatch work to a coding agent. The subagent runs acpx-rs, waits for the coding agent to finish, and the result is automatically announced back into your conversation context.

### sessions_spawn call

```
sessions_spawn(
  task: "<see task template below>",
  runtime: "subagent",
  mode: "run",
  label: "<agent>: <short description>"
)
```

### Task template

The `task` parameter must include explicit acpx-rs instructions. The subagent is a simple worker — be very explicit:

```
You are a worker subagent. Complete ALL steps sequentially. Do NOT stop early.

Step 1 — Ensure session exists:
Call exec with EXACTLY these parameters and NO others:
{"command": "~/.openclaw/workspace/skills/acpx-rs/acpx-rs sessions ensure --name <session-name> --agent \"<agent-command>\" --startup-timeout 60 --quiet", "timeout": 120}

Step 2 — Run prompt (blocks for minutes — this is normal, WAIT for it):
Call exec with EXACTLY these parameters and NO others:
{"command": "~/.openclaw/workspace/skills/acpx-rs/acpx-rs prompt -s <session-name> --summarize \"<user's task>\"", "timeout": 600}

If exec returns "Command still running", call process poll with timeout 580000 to wait for completion. Do NOT give up. Do NOT say "I'll relay later". WAIT.

Step 3 — Print the output from step 2 as your final message. Include session name "<session-name>".
```

## Agent commands and session names

| Agent | Session prefix | Command |
|-------|---------------|---------|
| Gemini | `oc-gemini-` | `gemini --experimental-acp --yolo -m auto-gemini-3` |
| Codex | `oc-codex-` | `npx -y @zed-industries/codex-acp` |
| Claude | `oc-claude-` | `npx -y @zed-industries/claude-agent-acp` |

Use the conversation/thread ID as suffix for session naming (e.g., `oc-gemini-1346`).

## Example: Dispatch to Gemini

```
sessions_spawn(
  task: "You are a worker subagent. Complete ALL steps sequentially. Do NOT stop early.\n\nStep 1 — Ensure session exists:\nCall exec with EXACTLY these parameters and NO others:\n{\"command\": \"~/.openclaw/workspace/skills/acpx-rs/acpx-rs sessions ensure --name oc-gemini-1346 --agent \\\"gemini --experimental-acp --yolo -m auto-gemini-3\\\" --startup-timeout 60 --quiet\", \"timeout\": 120}\n\nStep 2 — Run prompt (blocks for minutes — this is normal, WAIT for it):\nCall exec with EXACTLY these parameters and NO others:\n{\"command\": \"~/.openclaw/workspace/skills/acpx-rs/acpx-rs prompt -s oc-gemini-1346 --summarize \\\"check the deployment status\\\"\", \"timeout\": 600}\n\nIf exec returns \"Command still running\", call process poll with timeout 580000 to wait for completion. Do NOT give up. Do NOT say \"I'll relay later\". WAIT.\n\nStep 3 — Print the output from step 2 as your final message. Include session name \"oc-gemini-1346\".",
  runtime: "subagent",
  mode: "run",
  label: "gemini: check deployment status"
)
```

## After dispatching

1. Tell the user the session name and the subagent session ID (from sessions_spawn response): "Dispatched to `<agent>` (session: `<session-name>`, subagent: `<childSessionKey>`). I'll report back when it's done."
2. Continue handling other messages — the result will be announced back automatically.
3. When the result arrives, deliver it to the user and answer any follow-up questions.

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
