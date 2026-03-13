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
acpx prompt --session <SESSION> <TEXT...>
acpx status --session <SESSION>
acpx sessions ensure --name <NAME> --agent <COMMAND> [--cwd <DIR>] [--startup-timeout <SECS>] [--model <MODEL>] [--mode <MODE>]
acpx sessions last <NAME>
acpx sessions close <NAME>
```

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

## Project Status

Early, focused, and usable. The current implementation is intentionally small: one binary, one job, minimal ceremony.

If you want a lightweight ACP broker that is easy to inspect, script, and extend, this is the point of the project.
