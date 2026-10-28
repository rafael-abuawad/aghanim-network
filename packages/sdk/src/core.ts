import type { Persistence } from "./persistence.ts";
import { Engine, initWasm } from "./wasm.ts";

export type Effect =
  | { type: "send"; conn: number; data: Uint8Array }
  | { type: "changed"; paths: string[] }
  | { type: "ready"; conn: number; peer: string }
  | { type: "synced"; conn: number; scope: string }
  | { type: "close"; conn: number; reason: string };

/**
 * The engine's API as seen by the rest of the SDK. Implemented in-thread by
 * {@link LocalCore} and across a Worker boundary by `WorkerCore`.
 */
export interface CoreApi {
  deviceId(): Promise<string>;
  userId(): Promise<string | undefined>;
  login(alias: string, passphrase: string, remember: boolean): Promise<string>;
  loginWithSeed(seed: Uint8Array, remember: boolean): Promise<string>;
  createRandomUser(remember: boolean): Promise<string>;
  exportSeed(): Promise<Uint8Array | undefined>;
  logout(): Promise<void>;
  write(writes: Uint8Array): Promise<void>;
  get(path: string): Promise<Uint8Array | undefined>;
  query(query: Uint8Array): Promise<Uint8Array>;
  subscribe(scope: string): Promise<void>;
  connect(conn: number, sync: boolean, replicate: boolean): Promise<void>;
  disconnect(conn: number): Promise<void>;
  receive(conn: number, data: Uint8Array): Promise<void>;
  createSpace(name: string): Promise<string>;
  invite(space: string, member: string, role: string): Promise<void>;
  removeMember(space: string, member: string): Promise<void>;
  members(space: string): Promise<Uint8Array>;
  rawDoc(path: string): Promise<Uint8Array | undefined>;
  /** Resolves once every write so far is durable. */
  flush(): Promise<void>;
  close(): Promise<void>;
  onEffects(cb: (fx: Effect[]) => void): void;
}

/** Runs the WASM engine in the current thread. */
export class LocalCore implements CoreApi {
  private listeners: ((fx: Effect[]) => void)[] = [];
  private persisting: Promise<void> = Promise.resolve();
  private persistError: unknown;

  private constructor(
    private engine: Engine,
    private persistence: Persistence,
  ) {}

  static async open(appId: string, persistence: Persistence, wasm?: BufferSource | URL | string): Promise<LocalCore> {
    await initWasm(wasm);
    const [keys, values] = await persistence.load();
    const core = new LocalCore(new Engine(appId, keys, values), persistence);
    core.persist(); // the device key, on first run
    return core;
  }

  private persist(): void {
    const journal = this.engine.takeJournal() as [Uint8Array, Uint8Array | null][];
    if (journal.length === 0) return;
    this.persisting = this.persisting
      .then(() => this.persistence.apply(journal))
      .catch((e) => {
        this.persistError = e;
        console.error("aghanim: failed to persist", e);
      });
  }

  /** Runs an engine call, persists its writes, and dispatches its effects. */
  private run<T>(f: () => T, effects?: (r: T) => Effect[] | undefined): T {
    const result = f();
    this.persist();
    const fx = effects ? effects(result) : undefined;
    if (fx && fx.length > 0) for (const l of this.listeners) l(fx);
    return result;
  }

  private fx(f: () => Effect[]): void {
    this.run(f, (r) => r);
  }

  onEffects(cb: (fx: Effect[]) => void): void {
    this.listeners.push(cb);
  }

  async deviceId() {
    return this.engine.deviceId();
  }
  async userId() {
    return this.engine.userId();
  }
  async login(alias: string, passphrase: string, remember: boolean) {
    return this.run(() => this.engine.login(alias, passphrase, remember));
  }
  async loginWithSeed(seed: Uint8Array, remember: boolean) {
    return this.run(() => this.engine.loginWithSeed(seed, remember));
  }
  async createRandomUser(remember: boolean) {
    return this.run(() => this.engine.createRandomUser(remember));
  }
  async exportSeed() {
    return this.engine.exportSeed();
  }
  async logout() {
    this.run(() => this.engine.logout());
  }
  async write(writes: Uint8Array) {
    this.fx(() => this.engine.write(writes, Date.now()));
  }
  async get(path: string) {
    return this.engine.get(path);
  }
  async query(query: Uint8Array) {
    return this.engine.query(query);
  }
  async subscribe(scope: string) {
    this.fx(() => this.engine.subscribe(scope));
  }
  async connect(conn: number, sync: boolean, replicate: boolean) {
    this.fx(() => this.engine.connect(conn, sync, replicate));
  }
  async disconnect(conn: number) {
    this.engine.disconnect(conn);
  }
  async receive(conn: number, data: Uint8Array) {
    this.fx(() => this.engine.receive(conn, data, Date.now()));
  }
  async createSpace(name: string) {
    const [id] = this.run(
      () => this.engine.createSpace(name, Date.now()) as [string, Effect[]],
      (r) => r[1],
    );
    return id;
  }
  async invite(space: string, member: string, role: string) {
    this.fx(() => this.engine.invite(space, member, role, Date.now()));
  }
  async removeMember(space: string, member: string) {
    this.fx(() => this.engine.removeMember(space, member, Date.now()));
  }
  async members(space: string) {
    return this.engine.members(space);
  }
  async rawDoc(path: string) {
    return this.engine.rawDoc(path);
  }
  async flush() {
    await this.persisting;
    if (this.persistError) throw this.persistError;
  }
  async close() {
    await this.persisting;
    this.persistence.close();
    this.engine.free();
  }
}
