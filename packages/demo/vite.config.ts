import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

export default defineConfig({
  server: {
    port: Number(process.env.PORT ?? 5173),
    strictPort: true,
    fs: { allow: [fileURLToPath(new URL("../..", import.meta.url))] },
  },
  worker: { format: "es" },
  // The SDK ships TypeScript sources and a .wasm next to its bindings; let
  // Vite process it instead of pre-bundling.
  optimizeDeps: { exclude: ["@aghanim/sdk"] },
  build: {
    target: "es2022",
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL("index.html", import.meta.url)),
        test: fileURLToPath(new URL("test.html", import.meta.url)),
      },
    },
  },
});
