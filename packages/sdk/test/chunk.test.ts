import { expect, it } from "vitest";
import { chunk, Reassembler } from "../src/net/webrtc.ts";

it("splits and reassembles DataChannel frames", () => {
  for (const len of [0, 1, 100, 16 * 1024 - 1, 16 * 1024, 100_000]) {
    const data = new Uint8Array(len).map((_, i) => i % 251);
    const parts = chunk(data);
    expect(parts.every((p) => p.length <= 16 * 1024)).toBe(true);
    const r = new Reassembler();
    let out: Uint8Array | undefined;
    for (const p of parts) out = r.push(p) ?? out;
    expect(out).toEqual(data);
  }
});
