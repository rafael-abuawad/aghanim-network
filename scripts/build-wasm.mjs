// Builds the Rust engine to WebAssembly and generates its JS bindings into
// packages/sdk/wasm. Uses cargo + wasm-bindgen directly (no wasm-pack), and
// wasm-opt when it is installed.
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import { gzipSync } from "node:zlib";

const run = (cmd, args) => execFileSync(cmd, args, { stdio: "inherit" });
const has = (cmd) => {
  try {
    execFileSync(cmd, ["--version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
};

const out = "packages/sdk/wasm";
run("cargo", ["build", "-p", "aghanim-wasm", "--target", "wasm32-unknown-unknown", "--release"]);
run("wasm-bindgen", ["--target", "web", "--out-dir", out, "target/wasm32-unknown-unknown/release/aghanim_wasm.wasm"]);
const wasm = `${out}/aghanim_wasm_bg.wasm`;
if (has("wasm-opt")) {
  run("wasm-opt", [
    "-Os",
    "--enable-bulk-memory",
    "--enable-nontrapping-float-to-int",
    "--enable-sign-ext",
    "--enable-mutable-globals",
    "-o",
    wasm,
    wasm,
  ]);
} else {
  console.warn("wasm-opt not found; skipping size optimization");
}
if (!existsSync(wasm)) throw new Error(`missing ${wasm}`);
const gz = gzipSync(readFileSync(wasm), { level: 9 }).length;
console.log(`${wasm}: ${(statSync(wasm).size / 1024).toFixed(0)} KiB, ${(gz / 1024).toFixed(0)} KiB gzipped`);
if (gz > 400 * 1024) {
  console.error("WASM exceeds the 400 KiB gzipped budget");
  process.exit(1);
}
