#!/usr/bin/env node
import { spawn } from "node:child_process";

const env = { ...process.env };
const args = process.argv.slice(2);

const child = spawn("npx", ["-y", "@zed-industries/codex-acp@latest", ...args], {
  env,
  stdio: "inherit",
});

child.on("exit", (code) => process.exit(code ?? 0));
