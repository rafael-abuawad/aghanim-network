import { fileURLToPath } from "node:url";
import { defineConfig } from "@playwright/test";

export const RELAY_PORT = 18787;
export const WEB_PORT = 15173;
export const RELAY = `ws://127.0.0.1:${RELAY_PORT}`;
export const DATA_DIR = fileURLToPath(new URL("./.relay-data", import.meta.url));
const root = fileURLToPath(new URL("../..", import.meta.url));

export default defineConfig({
  testDir: ".",
  timeout: 60_000,
  workers: 2,
  reporter: [["list"]],
  use: {
    baseURL: `http://127.0.0.1:${WEB_PORT}`,
    // Chrome hides host ICE candidates behind mDNS names by default; turn
    // that off so two headless contexts can connect over localhost.
    launchOptions: { args: ["--disable-features=WebRtcHideLocalIpsWithMdns"] },
  },
  webServer: [
    {
      command: `rm -rf ${DATA_DIR} && cargo run -q -p aghanim-relay -- --listen 127.0.0.1:${RELAY_PORT} --data-dir ${DATA_DIR} --no-ice`,
      url: `http://127.0.0.1:${RELAY_PORT}/health`,
      cwd: root,
      timeout: 300_000,
      reuseExistingServer: false,
      stdout: "ignore",
      stderr: "pipe",
    },
    {
      command: `pnpm --filter @aghanim/demo exec vite --host 127.0.0.1`,
      env: { PORT: String(WEB_PORT) },
      url: `http://127.0.0.1:${WEB_PORT}/test.html`,
      cwd: root,
      timeout: 120_000,
      reuseExistingServer: false,
    },
  ],
});
