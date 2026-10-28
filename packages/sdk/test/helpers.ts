import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { type Db, type LinkHandle, type OpenOptions, openDb } from "../src/index.ts";

const wasm = readFileSync(fileURLToPath(new URL("../wasm/aghanim_wasm_bg.wasm", import.meta.url)));

export function open(extra: Partial<OpenOptions> = {}): Promise<Db> {
  return openDb({
    appId: "vitest",
    worker: false,
    webrtc: false,
    tabs: false,
    persistence: "memory",
    wasm,
    ...extra,
  });
}

/** Connects two databases with an in-memory, ordered pipe. */
export function pair(a: Db, b: Db): { cut(): void } {
  let ha: LinkHandle | undefined;
  let hb: LinkHandle | undefined;
  let up = true;
  ha = a.network.addLink({
    label: "to-b",
    send: (d) => up && queueMicrotask(() => hb?.deliver(d.slice())),
    close: () => cut(),
  });
  hb = b.network.addLink({
    label: "to-a",
    send: (d) => up && queueMicrotask(() => ha?.deliver(d.slice())),
    close: () => cut(),
  });
  function cut() {
    if (!up) return;
    up = false;
    ha?.closed();
    hb?.closed();
  }
  return { cut };
}

/** Polls until `f` returns a truthy value. */
export async function until<T>(f: () => T | Promise<T>, ms = 3000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v) return v;
    if (Date.now() > end) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 10));
  }
}
