import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import type {
  AcpRuntime,
  AcpRuntimeCapabilities,
  AcpRuntimeDoctorReport,
  AcpRuntimeEnsureInput,
  AcpRuntimeEvent,
  AcpRuntimeHandle,
  AcpRuntimeStatus,
  AcpRuntimeTurnInput,
  PluginLogger,
} from "openclaw/plugin-sdk/acpx";
import { AcpRuntimeError } from "openclaw/plugin-sdk/acpx";

export const BACKEND_ID = "acpx-rs";

type AgentConfig = {
  command: string;
  cwd?: string;
  model?: string;
  mode?: string;
};

type HandleState = {
  sessionName: string;
  agent: string;
  cwd?: string;
};

export class AcpxRsRuntime implements AcpRuntime {
  readonly command: string;
  private agents: Record<string, AgentConfig>;
  private defaultModel?: string;
  private defaultMode?: string;
  private startupTimeout: number;
  private logger?: PluginLogger;
  private healthy = false;

  constructor(config: Record<string, unknown>, logger?: PluginLogger) {
    this.command = (config.command as string) ?? "/usr/local/bin/acpx-rs";
    this.agents = (config.agents as Record<string, AgentConfig>) ?? {};
    this.defaultModel = config.defaultModel as string | undefined;
    this.defaultMode = config.defaultMode as string | undefined;
    this.startupTimeout = (config.startupTimeout as number) ?? 60;
    this.logger = logger;
  }

  isHealthy(): boolean {
    return this.healthy;
  }

  async probeAvailability(): Promise<void> {
    try {
      await this.exec(["--version"]);
      this.healthy = true;
    } catch {
      this.healthy = false;
    }
  }

  async ensureSession(input: AcpRuntimeEnsureInput): Promise<AcpRuntimeHandle> {
    const agentConfig = this.agents[input.agent];
    if (!agentConfig) {
      throw new AcpRuntimeError(
        "ACP_SESSION_INIT_FAILED",
        `Unknown agent "${input.agent}". Configure it in the acpx-rs plugin config under "agents".`,
      );
    }

    const args = [
      "sessions",
      "ensure",
      "--name",
      input.sessionKey,
      "--agent",
      agentConfig.command,
      "--startup-timeout",
      String(this.startupTimeout),
    ];

    const cwd = agentConfig.cwd ?? input.cwd;
    if (cwd) {
      args.push("--cwd", cwd);
    }

    const model = agentConfig.model ?? this.defaultModel;
    if (model) {
      args.push("--model", model);
    }

    const mode = agentConfig.mode ?? this.defaultMode;
    if (mode) {
      args.push("--mode", mode);
    }

    const output = await this.exec(args);
    let record: Record<string, unknown>;
    try {
      record = JSON.parse(output);
    } catch {
      throw new AcpRuntimeError(
        "ACP_SESSION_INIT_FAILED",
        `Failed to parse session record: ${output.slice(0, 200)}`,
      );
    }

    const state: HandleState = {
      sessionName: input.sessionKey,
      agent: input.agent,
      cwd,
    };

    return {
      sessionKey: input.sessionKey,
      backend: BACKEND_ID,
      runtimeSessionName: Buffer.from(JSON.stringify(state)).toString("base64url"),
      cwd,
      agentSessionId: record.acp_session_id as string | undefined,
    };
  }

  async *runTurn(input: AcpRuntimeTurnInput): AsyncIterable<AcpRuntimeEvent> {
    const state = this.decodeHandle(input.handle);

    const args = [
      "prompt",
      "-s",
      state.sessionName,
      "--json",
      input.text,
    ];

    const child = spawn(this.command, args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, CLAUDECODE: undefined },
    });

    if (input.signal) {
      input.signal.addEventListener("abort", () => {
        child.kill("SIGTERM");
      });
    }

    // Capture stderr so agent/CLI errors are not lost
    let stderrBuf = "";
    child.stderr?.on("data", (data: Buffer) => {
      stderrBuf += data.toString();
      if (stderrBuf.length > 4096) {
        stderrBuf = stderrBuf.slice(-4096);
      }
    });

    // Heartbeat: emit a lightweight status event when no events have been
    // forwarded for HEARTBEAT_INTERVAL_MS so ACP knows the task is alive.
    const HEARTBEAT_INTERVAL_MS = 15_000;
    let lastYieldAt = Date.now();
    const pendingHeartbeats: AcpRuntimeEvent[] = [];
    const heartbeat = setInterval(() => {
      if (Date.now() - lastYieldAt >= HEARTBEAT_INTERVAL_MS) {
        pendingHeartbeats.push({
          type: "status",
          text: "working…",
          tag: "usage_update",
        } as AcpRuntimeEvent);
      }
    }, HEARTBEAT_INTERVAL_MS);

    // Watchdog: if no events for WATCHDOG_INTERVAL_MS, poll session status.
    // If the session is closed/dead, kill the child so the readline loop ends
    // and we can report the actual error instead of hanging.
    const WATCHDOG_INTERVAL_MS = 30_000;
    let lastEventAt = Date.now();
    let watchdogReason: string | null = null;
    let watchdogRunning = false;
    const watchdog = setInterval(async () => {
      if (watchdogRunning) return;
      if (Date.now() - lastEventAt < WATCHDOG_INTERVAL_MS) return;
      watchdogRunning = true;
      try {
        const output = await this.exec(["status", "-s", state.sessionName]);
        const record = JSON.parse(output) as Record<string, unknown>;
        if (record.closed) {
          watchdogReason =
            (record.death_reason as string) || "session closed (detected by watchdog)";
          child.kill("SIGTERM");
        }
      } catch {
        watchdogReason = "session unreachable (detected by watchdog)";
        child.kill("SIGTERM");
      } finally {
        watchdogRunning = false;
      }
    }, WATCHDOG_INTERVAL_MS);

    const rl = createInterface({ input: child.stdout });
    let hadError: string | null = null;

    try {
      for await (const line of rl) {
        lastEventAt = Date.now();

        // Flush any pending heartbeats before the real event
        while (pendingHeartbeats.length > 0) {
          yield pendingHeartbeats.shift()!;
        }

        const trimmed = line.trim();
        if (!trimmed) continue;

        let event: Record<string, unknown>;
        try {
          event = JSON.parse(trimmed);
        } catch {
          continue;
        }

        const type = event.type as string;

        if (type === "text_delta" || type === "done" || type === "error") {
          if (type === "error") hadError = event.message as string;
          lastYieldAt = Date.now();
          yield event as AcpRuntimeEvent;
          if (type === "done" || type === "error") return;
        } else if (type === "status") {
          lastYieldAt = Date.now();
          yield {
            type: "status",
            text: (event.text as string) ?? JSON.stringify(event),
          } as AcpRuntimeEvent;
        }
      }

      // If we get here without done/error, the process ended unexpectedly
      if (!hadError) {
        const detail = watchdogReason || stderrBuf.trim();
        const message = detail
          ? `acpx-rs failed: ${detail.slice(0, 500)}`
          : "acpx-rs process ended without a done event";
        yield {
          type: "error",
          message,
        };
      }
    } finally {
      clearInterval(heartbeat);
      clearInterval(watchdog);
      rl.close();
      child.kill("SIGTERM");
    }
  }

  getCapabilities(): AcpRuntimeCapabilities {
    return {
      controls: ["session/set_mode", "session/set_config_option", "session/status"],
    };
  }

  async getStatus(input: { handle: AcpRuntimeHandle }): Promise<AcpRuntimeStatus> {
    const state = this.decodeHandle(input.handle);
    try {
      const output = await this.exec(["status", "-s", state.sessionName]);
      const record = JSON.parse(output);
      return {
        summary: record.closed ? "closed" : "running",
        agentSessionId: record.acp_session_id,
        details: record,
      };
    } catch {
      return { summary: "unknown" };
    }
  }

  async cancel(input: { handle: AcpRuntimeHandle }): Promise<void> {
    // acpx-rs doesn't have a cancel command yet; close the session instead
    const state = this.decodeHandle(input.handle);
    try {
      await this.exec(["sessions", "close", state.sessionName]);
    } catch {
      // ignore close errors
    }
  }

  async close(input: { handle: AcpRuntimeHandle }): Promise<void> {
    const state = this.decodeHandle(input.handle);
    await this.exec(["sessions", "close", state.sessionName]);
  }

  async doctor(): Promise<AcpRuntimeDoctorReport> {
    try {
      const output = await this.exec(["--version"]);
      return {
        ok: true,
        message: `acpx-rs available: ${output.trim()}`,
      };
    } catch (err) {
      return {
        ok: false,
        code: "ACPX_RS_NOT_FOUND",
        message: `acpx-rs binary not found at "${this.command}"`,
        installCommand: "cd ~/repos/acpx-rs && cargo build --release",
        details: [err instanceof Error ? err.message : String(err)],
      };
    }
  }

  private decodeHandle(handle: AcpRuntimeHandle): HandleState {
    try {
      const json = Buffer.from(handle.runtimeSessionName, "base64url").toString();
      return JSON.parse(json);
    } catch {
      // Fallback: treat runtimeSessionName as plain session name
      return {
        sessionName: handle.runtimeSessionName,
        agent: "unknown",
      };
    }
  }

  private exec(args: string[]): Promise<string> {
    return new Promise((resolve, reject) => {
      const child = spawn(this.command, args, {
        stdio: ["ignore", "pipe", "pipe"],
        env: { ...process.env, CLAUDECODE: undefined },
      });

      let stdout = "";
      let stderr = "";

      child.stdout.on("data", (data: Buffer) => {
        stdout += data.toString();
      });
      child.stderr.on("data", (data: Buffer) => {
        stderr += data.toString();
      });
      child.on("error", (err) => reject(err));
      child.on("close", (code) => {
        if (code === 0) {
          resolve(stdout);
        } else {
          reject(new Error(`acpx-rs exited ${code}: ${stderr || stdout}`));
        }
      });
    });
  }
}
