import init, { Engine, initSync } from "../wasm/aghanim_wasm.js";

export { Engine };

let ready: Promise<void> | undefined;

/**
 * Loads the WebAssembly engine once. In browsers and workers the module is
 * fetched next to this file; pass bytes or a URL to load it yourself (Node,
 * custom CDNs).
 */
export function initWasm(source?: BufferSource | URL | string): Promise<void> {
  if (!ready) {
    ready =
      source instanceof ArrayBuffer || ArrayBuffer.isView(source)
        ? Promise.resolve(void initSync({ module: source }))
        : init(source === undefined ? undefined : { module_or_path: source }).then(() => undefined);
    ready.catch(() => {
      ready = undefined;
    });
  }
  return ready;
}
