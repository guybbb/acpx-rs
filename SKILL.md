---
name: acpx-rs
description: ACP session broker. Dispatch work to Codex, Claude Code, Gemini, or the investment agent. Call directly via exec — results return in your context.
user-invocable: false
---

# acpx-rs — ACP Session Broker

Manages long-running coding agent sessions. You call it directly via exec — no subagent needed.

## How to dispatch work

Three steps. Use ONLY \`command\` and \`timeout\` parameters — no other exec parameters.

### Step 1 — Ensure session exists

\`\`\`
exec(command: "acpx-rs sessions ensure --name <session-name> --agent \"<agent-command>\" --cwd /home/openclaw --startup-timeout 60 --quiet", timeout: 120)
\`\`\`

### Step 2 — Health check (CRITICAL — do this EVERY time after ensure)

\`\`\`
exec(command: "acpx-rs status -s <session-name>", timeout: 10)
\`\`\`

If status is NOT \`running\`, **STOP and tell the user immediately**. Include the status and death_reason. Do NOT send a prompt to a dead session.

### Step 3 — Run the prompt (blocks 1–10 min, this is normal — WAIT for it)

\`\`\`
exec(command: "acpx-rs prompt -s <session-name> --summarize \"<user's task>\"", timeout: 600)
\`\`\`

### Step 4 — Deliver the output from step 3 to the user. Include the session name.

If step 3 returns an error, tell the user what went wrong immediately. Do NOT silently ignore errors.

## Agent commands and session names

| Agent | Session prefix | Command |
|-------|---------------|---------|
| Gemini | \`oc-gemini-\` | \`gemini --experimental-acp --yolo -m gemini-3-flash-preview\` |
| Codex | \`oc-codex-\` | \`env CODEX_HOME=/home/openclaw/.codex npx -y @zed-industries/codex-acp@latest\` |
| Claude | \`oc-claude-\` | \`npx -y @agentclientprotocol/claude-agent-acp@latest\` |
| Investment | \`oc-invest-\` | \`/home/openclaw/.openclaw/workspace-trading/trading-app/.venv/bin/python /home/openclaw/.openclaw/workspace-trading/trading-app/run.py --investment-agent-acp\` |

Use the conversation/thread ID as suffix (e.g., \`oc-gemini-1346\`).

## Important rules

- Do NOT use \`sessions_spawn\` or subagents — call exec directly.
- Do NOT add \`yieldMs\`, \`background\`, \`security\`, \`host\`, \`ask\`, or \`pty\` to exec calls.
- The prompt command blocks while the coding agent works. This is expected. Wait for it.
- Always include \`--cwd /home/openclaw\` in the ensure command.
- ALWAYS run the health check after ensure. Report dead/stale sessions to the user immediately.

## Diagnostic commands (exec)

\`\`\`bash
ACPX=acpx-rs
\${ACPX} status -s <session-name>
\${ACPX} sessions list --active
\${ACPX} sessions last <session-name>
\${ACPX} sessions close <session-name>
\`\`\`

## Session lifecycle

- \`sessions ensure\` is idempotent — reuses alive sessions, recreates dead ones
- Sessions persist across tasks (agent retains context)
- Idle timeout: 30 minutes
- Do NOT close sessions after each task
