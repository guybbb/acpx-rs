#!/usr/bin/env node
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const env = { ...process.env };
const args = process.argv.slice(2);

// Standard Claude Code entry point via npx
const child = spawn("npx", ["-y", "@agentclientprotocol/claude-agent-acp@latest", ...args], {
  env,
  stdio: "inherit",
});

child.on("exit", (code) => process.exit(code ?? 0));
