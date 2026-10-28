import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const page = (path: string) => fileURLToPath(new URL(path, import.meta.url));

export default defineConfig({
  server: {
    port: Number(process.env.PORT ?? 5174),
    strictPort: true,
    fs: { allow: [fileURLToPath(new URL("..", import.meta.url))] },
  },
  worker: { format: "es" },
  // The SDK ships TypeScript sources and a .wasm next to its bindings; let
  // Vite process it instead of pre-bundling.
  optimizeDeps: { exclude: ["@aghanim/sdk"] },
  build: {
    target: "es2022",
    chunkSizeWarningLimit: 700,
    rollupOptions: {
      input: {
        scripts: page("index.html"),
        chat: page("chat/index.html"),
        discord: page("discord/index.html"),
      },
    },
  },
});
