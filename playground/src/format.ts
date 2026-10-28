import { DocumentSnapshot, QuerySnapshot } from "@aghanim/sdk";

function hex(bytes: Uint8Array, max: number): string {
  let s = "";
  for (const b of bytes.subarray(0, max)) s += b.toString(16).padStart(2, "0");
  return bytes.length > max ? `${s}…` : s;
}

function replacer(_key: string, v: unknown): unknown {
  if (typeof v === "bigint") return `${v}n`;
  if (v === undefined) return "undefined";
  if (v instanceof Uint8Array) return `<${v.length} bytes ${hex(v, 16)}>`;
  if (v instanceof Map) return Object.fromEntries(v);
  if (v instanceof DocumentSnapshot) return { id: v.id, exists: v.exists(), data: v.data() };
  if (v instanceof QuerySnapshot) return v.docs.map((d) => ({ id: d.id, ...d.data() }));
  return v;
}

/** Renders a logged value the way a developer console would, roughly. */
export function format(v: unknown): string {
  if (typeof v === "string") return v;
  if (v instanceof Error) return v.message;
  if (v === undefined || typeof v === "function" || typeof v === "symbol") return String(v);
  const compact = JSON.stringify(v, replacer);
  return compact.length <= 72 ? compact : JSON.stringify(v, replacer, 2);
}
