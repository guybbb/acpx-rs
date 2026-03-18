import type { OpenClawPluginApi } from "openclaw/plugin-sdk/acpx";
import { createAcpxRsService } from "./src/service.js";

const plugin = {
  id: "acpx-rs",
  name: "ACPX-RS Runtime",
  description: "ACP runtime backend powered by the acpx-rs Rust broker.",
  register(api: OpenClawPluginApi) {
    api.registerService(
      createAcpxRsService({ pluginConfig: api.pluginConfig }),
    );
  },
};

export default plugin;
