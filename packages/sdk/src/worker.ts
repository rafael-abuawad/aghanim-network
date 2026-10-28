/// <reference lib="webworker" />
// Worker entry: hosts the WASM engine and its IndexedDB persistence off the
// main thread. The main thread talks to it through `WorkerCore`.
import { type Effect, LocalCore } from "./core.ts";
import { IdbPersistence, MemoryPersistence } from "./persistence.ts";

declare const self: DedicatedWorkerGlobalScope;

let core: LocalCore | undefined;

function transfers(fx: Effect[]): Transferable[] {
  return fx.flatMap((e) => (e.type === "send" ? [e.data.buffer as ArrayBuffer] : []));
}

self.onmessage = async (ev: MessageEvent<{ id: number; method: string; args: unknown[] }>) => {
  const { id, method, args } = ev.data;
  try {
    let result: unknown;
    if (method === "open") {
      const [appId, persistence, dbName] = args as [string, "indexeddb" | "memory", string];
      const store = persistence === "indexeddb" ? await IdbPersistence.open(dbName) : new MemoryPersistence();
      core = await LocalCore.open(appId, store);
      core.onEffects((fx) => self.postMessage({ effects: fx }, transfers(fx)));
    } else {
      if (!core) throw new Error("engine not open");
      const fn = (core as unknown as Record<string, (...a: unknown[]) => Promise<unknown>>)[method];
      if (typeof fn !== "function") throw new Error(`unknown method ${method}`);
      result = await fn.apply(core, args);
    }
    self.postMessage({ id, ok: true, result });
  } catch (e) {
    self.postMessage({ id, ok: false, error: e instanceof Error ? e.message : String(e) });
  }
};
