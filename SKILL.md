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

The `task` parameter must include acpx-rs instructions so the subagent knows how to run the coding agent:

```
Run the following task using <Agent> via acpx-rs:

Task: <user's task description>

Instructions:
1. Run: exec ~/.openclaw/workspace/skills/acpx-rs/acpx-rs sessions ensure --name <session-name> --agent "<agent-command>" --startup-timeout 60 --quiet
2. Run: exec ~/.openclaw/workspace/skills/acpx-rs/acpx-rs prompt -s <session-name> --summarize "<user's task>"
3. The prompt command will block until the agent finishes and print a summary. Report the summary as your final message.

If step 1 or 2 fails, report the error.
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
  task: "Run the following task using Gemini via acpx-rs:\n\nTask: check the deployment status\n\nInstructions:\n1. Run: exec ~/.openclaw/workspace/skills/acpx-rs/acpx-rs sessions ensure --name oc-gemini-1346 --agent \"gemini --experimental-acp --yolo -m auto-gemini-3\" --startup-timeout 60 --quiet\n2. Run: exec ~/.openclaw/workspace/skills/acpx-rs/acpx-rs prompt -s oc-gemini-1346 --summarize \"check the deployment status\"\n3. The prompt command will block until the agent finishes and print a summary. Report the summary as your final message.\n\nIf step 1 or 2 fails, report the error.",
  runtime: "subagent",
  mode: "run",
  label: "gemini: check deployment status"
)
```

## After dispatching

1. Tell the user: "Dispatched to `<agent>`. I'll report back when it's done."
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
