// Firestore-style modular API: collection(), doc(), query(), setDoc(),
// onSnapshot()… Paths always start with a scope: `public/<name>`,
// `users/<pubkey>`, or `spaces/<spaceId>`.
import { decode, encode } from "./cbor.ts";
import type { Db } from "./db.ts";
import { report } from "./net/manager.ts";

export type DocumentData = { [field: string]: unknown };
export type WhereOp = "==" | "!=" | "<" | "<=" | ">" | ">=" | "in";
export type Direction = "asc" | "desc";

function splitPath(path: string): string[] {
  const segs = path.split("/").filter((s) => s.length > 0);
  if (segs.length === 0) throw new Error("empty path");
  return segs;
}

function scopeOf(path: string): string {
  const segs = splitPath(path);
  if (segs.length < 2) throw new Error(`path ${path} must start with a scope like public/<name>`);
  return `${segs[0]}/${segs[1]}`;
}

function parentOf(path: string): string {
  return path.slice(0, path.lastIndexOf("/"));
}

const ID_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/** A random 20-character document id. */
export function newId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(20));
  let id = "";
  for (const b of bytes) id += ID_CHARS[b % ID_CHARS.length];
  return id;
}

export class Query {
  /** @internal */
  constructor(
    readonly db: Db,
    readonly path: string,
    /** @internal */ readonly filters: [string, WhereOp, unknown][] = [],
    /** @internal */ readonly order?: [string, Direction],
    /** @internal */ readonly max?: number,
  ) {}

  /** @internal */
  toWire(): Uint8Array {
    return encode({
      collection: this.path,
      where: this.filters.map(([f, op, v]) => [f, op, toWireValue(v)]),
      orderBy: this.order ?? null,
      limit: this.max ?? null,
    });
  }
}

export class CollectionReference extends Query {
  get id(): string {
    return this.path.slice(this.path.lastIndexOf("/") + 1);
  }

  get parent(): DocumentReference {
    return new DocumentReference(this.db, parentOf(this.path));
  }
}

export class DocumentReference {
  constructor(
    readonly db: Db,
    readonly path: string,
  ) {
    if (splitPath(path).length % 2 !== 0) throw new Error(`${path} is not a document path`);
  }

  get id(): string {
    return this.path.slice(this.path.lastIndexOf("/") + 1);
  }

  get parent(): CollectionReference {
    return new CollectionReference(this.db, parentOf(this.path));
  }
}

/** A collection, e.g. `collection(db, "public/chat/messages")`. */
export function collection(base: Db | DocumentReference, path: string, ...more: string[]): CollectionReference {
  const full = [base instanceof DocumentReference ? base.path : "", path, ...more].filter(Boolean).join("/");
  if (splitPath(full).length % 2 !== 1) throw new Error(`${full} is not a collection path`);
  return new CollectionReference(base instanceof DocumentReference ? base.db : base, full);
}

/**
 * A document: `doc(db, "public/chat/messages/m1")`, `doc(coll, "m1")`, or
 * `doc(coll)` for a new random id.
 */
export function doc(base: Db | CollectionReference, path?: string, ...more: string[]): DocumentReference {
  if (base instanceof CollectionReference) {
    return new DocumentReference(base.db, [base.path, path ?? newId(), ...more].join("/"));
  }
  if (!path) throw new Error("doc(db, path) needs a path");
  return new DocumentReference(base, [path, ...more].join("/"));
}

export type QueryConstraint =
  | { kind: "where"; field: string; op: WhereOp; value: unknown }
  | { kind: "orderBy"; field: string; dir: Direction }
  | { kind: "limit"; n: number };

export function where(field: string, op: WhereOp, value: unknown): QueryConstraint {
  return { kind: "where", field, op, value };
}

export function orderBy(field: string, dir: Direction = "asc"): QueryConstraint {
  return { kind: "orderBy", field, dir };
}

export function limit(n: number): QueryConstraint {
  if (!Number.isInteger(n) || n < 0) throw new Error("limit must be a non-negative integer");
  return { kind: "limit", n };
}

export function query(q: Query, ...constraints: QueryConstraint[]): Query {
  let filters = q.filters;
  let order = q.order;
  let max = q.max;
  for (const c of constraints) {
    if (c.kind === "where") filters = [...filters, [c.field, c.op, c.value]];
    else if (c.kind === "orderBy") {
      if (order) throw new Error("only one orderBy is supported");
      order = [c.field, c.dir];
    } else max = c.n;
  }
  return new Query(q.db, q.path, filters, order, max);
}

// ---------------------------------------------------------------------------
// Values

const DELETE = Symbol("aghanim.deleteField");

/** Use as a field value in `updateDoc`/`setDoc(…, {merge: true})` to delete it. */
export function deleteField(): unknown {
  return DELETE;
}

function toWireValue(v: unknown, depth = 0): unknown {
  if (depth > 32) throw new Error("value nested too deeply");
  if (v === null || typeof v === "boolean" || typeof v === "string") return v;
  if (typeof v === "number") {
    if (!Number.isFinite(v)) throw new Error(`unsupported number ${v}`);
    return v;
  }
  if (v instanceof Uint8Array) return v;
  if (v instanceof Date) return v.getTime();
  if (Array.isArray(v)) return v.map((x) => toWireValue(x === undefined ? null : x, depth + 1));
  if (v === DELETE) throw new Error("deleteField() can only be used as a top-level field value");
  if (typeof v === "object" && Object.getPrototypeOf(v) === Object.prototype) {
    const out: Record<string, unknown> = {};
    for (const [k, x] of Object.entries(v as object)) if (x !== undefined) out[k] = toWireValue(x, depth + 1);
    return out;
  }
  throw new Error(`unsupported value of type ${typeof v}`);
}

/** Top-level fields; `deleteField()` becomes CBOR `undefined` ("delete"). */
function toWireFields(data: DocumentData, allowDelete: boolean): Record<string, unknown> {
  if (data === null || typeof data !== "object" || Array.isArray(data))
    throw new Error("document data must be an object");
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(data)) {
    if (v === DELETE) {
      if (!allowDelete) throw new Error("deleteField() needs updateDoc or setDoc with {merge: true}");
      out[k] = undefined;
    } else if (v !== undefined) out[k] = toWireValue(v);
  }
  return out;
}

// ---------------------------------------------------------------------------
// Writes

type WireWrite = ["set", string, unknown, boolean] | ["update", string, unknown] | ["delete", string];

async function commit(db: Db, writes: WireWrite[]): Promise<void> {
  if (writes.length === 0) return;
  for (const w of writes) await db.ensureScope(scopeOf(w[1]));
  await db.core.write(encode(writes));
}

export function setDoc(ref: DocumentReference, data: DocumentData, opts: { merge?: boolean } = {}): Promise<void> {
  return commit(ref.db, [["set", ref.path, toWireFields(data, !!opts.merge), !!opts.merge]]);
}

export function updateDoc(ref: DocumentReference, data: DocumentData): Promise<void> {
  return commit(ref.db, [["update", ref.path, toWireFields(data, true)]]);
}

export function deleteDoc(ref: DocumentReference): Promise<void> {
  return commit(ref.db, [["delete", ref.path]]);
}

export async function addDoc(coll: CollectionReference, data: DocumentData): Promise<DocumentReference> {
  const ref = doc(coll);
  await setDoc(ref, data);
  return ref;
}

export class WriteBatch {
  private writes: WireWrite[] = [];
  private done = false;

  constructor(private db: Db) {}

  set(ref: DocumentReference, data: DocumentData, opts: { merge?: boolean } = {}): this {
    this.writes.push(["set", ref.path, toWireFields(data, !!opts.merge), !!opts.merge]);
    return this;
  }

  update(ref: DocumentReference, data: DocumentData): this {
    this.writes.push(["update", ref.path, toWireFields(data, true)]);
    return this;
  }

  delete(ref: DocumentReference): this {
    this.writes.push(["delete", ref.path]);
    return this;
  }

  /** Applies all writes; writes within one scope are signed as one change. */
  commit(): Promise<void> {
    if (this.done) throw new Error("batch already committed");
    this.done = true;
    return commit(this.db, this.writes);
  }
}

export function writeBatch(db: Db): WriteBatch {
  return new WriteBatch(db);
}

// ---------------------------------------------------------------------------
// Reads and snapshots

export interface SnapshotMetadata {
  /** True until some peer has sent its backlog for this scope. */
  fromCache: boolean;
  /** Always false: local writes apply immediately. */
  hasPendingWrites: boolean;
}

export class DocumentSnapshot<T extends DocumentData = DocumentData> {
  constructor(
    readonly ref: DocumentReference,
    private fields: T | undefined,
    readonly metadata: SnapshotMetadata,
  ) {}

  get id(): string {
    return this.ref.id;
  }

  exists(): boolean {
    return this.fields !== undefined;
  }

  data(): T | undefined {
    return this.fields;
  }

  get(field: string): unknown {
    return this.fields?.[field];
  }
}

export class QueryDocumentSnapshot<T extends DocumentData = DocumentData> extends DocumentSnapshot<T> {
  override data(): T {
    return super.data() as T;
  }
}

export interface DocumentChange {
  type: "added" | "modified" | "removed";
  doc: QueryDocumentSnapshot;
  oldIndex: number;
  newIndex: number;
}

export class QuerySnapshot {
  constructor(
    readonly query: Query,
    readonly docs: QueryDocumentSnapshot[],
    private changes: DocumentChange[],
    readonly metadata: SnapshotMetadata,
  ) {}

  get size(): number {
    return this.docs.length;
  }

  get empty(): boolean {
    return this.docs.length === 0;
  }

  forEach(cb: (d: QueryDocumentSnapshot) => void): void {
    this.docs.forEach(cb);
  }

  /** What changed since the previous snapshot (everything is "added" first). */
  docChanges(): DocumentChange[] {
    return this.changes;
  }
}

function meta(db: Db, path: string): SnapshotMetadata {
  return { fromCache: !db.manager.isSynced(scopeOf(path)), hasPendingWrites: false };
}

export async function getDoc(ref: DocumentReference): Promise<DocumentSnapshot> {
  await ref.db.ensureScope(scopeOf(ref.path));
  const b = await ref.db.core.get(ref.path);
  return new DocumentSnapshot(ref, b ? decode<DocumentData>(b) : undefined, meta(ref.db, ref.path));
}

async function runQuery(q: Query): Promise<[QueryDocumentSnapshot, Uint8Array][]> {
  await q.db.ensureScope(scopeOf(q.path));
  const rows = decode<[string, DocumentData][]>(await q.db.core.query(q.toWire()));
  const m = meta(q.db, q.path);
  return rows.map(([path, fields]) => [
    new QueryDocumentSnapshot(new DocumentReference(q.db, path), fields, m),
    encode(fields),
  ]);
}

export async function getDocs(q: Query): Promise<QuerySnapshot> {
  const rows = await runQuery(q);
  const docs = rows.map((r) => r[0]);
  const changes = docs.map((doc, i) => ({ type: "added" as const, doc, oldIndex: -1, newIndex: i }));
  return new QuerySnapshot(q, docs, changes, meta(q.db, q.path));
}

function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

export type Unsubscribe = () => void;

/**
 * Listens to a document or query. `next` fires with the current state, then
 * on every local or remote change.
 */
export function onSnapshot(
  target: DocumentReference,
  next: (s: DocumentSnapshot) => void,
  error?: (e: Error) => void,
): Unsubscribe;
export function onSnapshot(target: Query, next: (s: QuerySnapshot) => void, error?: (e: Error) => void): Unsubscribe;
export function onSnapshot(
  target: DocumentReference | Query,
  next: ((s: DocumentSnapshot) => void) | ((s: QuerySnapshot) => void),
  error: (e: Error) => void = report,
): Unsubscribe {
  const db = target.db;
  let active = true;
  let generation = 0;
  const fail = (e: unknown) => error(e instanceof Error ? e : new Error(String(e)));

  if (target instanceof DocumentReference) {
    let last: Uint8Array | undefined | null = null;
    const cb = next as (s: DocumentSnapshot) => void;
    const remove = db.addListener({
      matches: (paths) => paths.includes(target.path),
      refresh: async () => {
        const gen = ++generation;
        try {
          const snap = await getDoc(target);
          const bytes = snap.exists() ? encode(snap.data()) : undefined;
          if (!active || gen !== generation) return;
          const same =
            last !== null && (last === undefined ? bytes === undefined : bytes !== undefined && sameBytes(last, bytes));
          if (same) return;
          last = bytes;
          cb(snap);
        } catch (e) {
          fail(e);
        }
      },
    });
    return () => {
      active = false;
      remove();
    };
  }

  const q = target;
  const cb = next as (s: QuerySnapshot) => void;
  let prev = new Map<string, { bytes: Uint8Array; index: number; doc: QueryDocumentSnapshot }>();
  let first = true;
  const remove = db.addListener({
    matches: (paths) => paths.some((p) => parentOf(p) === q.path),
    refresh: async () => {
      const gen = ++generation;
      try {
        const rows = await runQuery(q);
        if (!active || gen !== generation) return;
        const changes: DocumentChange[] = [];
        const next = new Map<string, { bytes: Uint8Array; index: number; doc: QueryDocumentSnapshot }>();
        for (const [i, [d, bytes]] of rows.entries()) next.set(d.ref.path, { bytes, index: i, doc: d });
        for (const [path, old] of prev) {
          if (!next.has(path)) changes.push({ type: "removed", doc: old.doc, oldIndex: old.index, newIndex: -1 });
        }
        for (const [path, cur] of next) {
          const old = prev.get(path);
          if (!old) changes.push({ type: "added", doc: cur.doc, oldIndex: -1, newIndex: cur.index });
          else if (!sameBytes(old.bytes, cur.bytes) || old.index !== cur.index) {
            changes.push({ type: "modified", doc: cur.doc, oldIndex: old.index, newIndex: cur.index });
          }
        }
        prev = next;
        if (!first && changes.length === 0) return;
        first = false;
        cb(
          new QuerySnapshot(
            q,
            rows.map((r) => r[0]),
            changes,
            meta(db, q.path),
          ),
        );
      } catch (e) {
        fail(e);
      }
    },
  });
  return () => {
    active = false;
    remove();
  };
}
