import { Encoder, type Options } from "cbor-x";

// Plain CBOR maps/arrays/byte strings, matching the Rust codec. Records and
// tags are off so every value round-trips through the engine unchanged.
// `int64AsNumber` is supported at runtime but missing from cbor-x's types.
const codec = new Encoder({
  useRecords: false,
  mapsAsObjects: true,
  tagUint8Array: false,
  variableMapSize: true,
  int64AsNumber: true,
} as Options & { int64AsNumber: boolean });

export function encode(value: unknown): Uint8Array {
  return codec.encode(value);
}

export function decode<T = unknown>(bytes: Uint8Array): T {
  return codec.decode(bytes) as T;
}
