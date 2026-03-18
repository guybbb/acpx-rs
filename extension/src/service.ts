import type {
  OpenClawPluginService,
  OpenClawPluginServiceContext,
} from "openclaw/plugin-sdk/acpx";
import {
  registerAcpRuntimeBackend,
  unregisterAcpRuntimeBackend,
} from "openclaw/plugin-sdk/acpx";
import { AcpxRsRuntime, BACKEND_ID } from "./runtime.js";

type CreateServiceParams = {
  pluginConfig?: unknown;
};

export function createAcpxRsService(
  params: CreateServiceParams = {},
): OpenClawPluginService {
  let runtime: AcpxRsRuntime | null = null;

  return {
    id: "acpx-rs-runtime",
    async start(ctx: OpenClawPluginServiceContext): Promise<void> {
      const config = (params.pluginConfig ?? {}) as Record<string, unknown>;
      runtime = new AcpxRsRuntime(config, ctx.logger);

      registerAcpRuntimeBackend({
        id: BACKEND_ID,
        runtime,
        healthy: () => runtime?.isHealthy() ?? false,
      });

      const command = runtime.command;
      ctx.logger.info(`acpx-rs runtime backend registered (command: ${command})`);

      // Probe availability in background
      void runtime.probeAvailability().then(
        () => {
          if (runtime?.isHealthy()) {
            ctx.logger.info("acpx-rs runtime backend ready");
          }
        },
        (err) => {
          ctx.logger.warn(
            `acpx-rs probe failed: ${err instanceof Error ? err.message : String(err)}`,
          );
        },
      );
    },
    async stop(_ctx: OpenClawPluginServiceContext): Promise<void> {
      unregisterAcpRuntimeBackend(BACKEND_ID);
      runtime = null;
    },
  };
}
