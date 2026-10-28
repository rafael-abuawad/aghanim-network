import type { CoreApi, Effect } from "./core.ts";

type Pending = { resolve: (v: unknown) => void; reject: (e: Error) => void };

/** {@link CoreApi} backed by the engine running in a dedicated Worker. */
export class WorkerCore implements CoreApi {
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private listeners: ((fx: Effect[]) => void)[] = [];

  private constructor(private worker: Worker) {
    worker.onmessage = (ev: MessageEvent) => {
      const m = ev.data;
      if (m.effects) {
        for (const l of this.listeners) l(m.effects as Effect[]);
        return;
      }
      const p = this.pending.get(m.id);
      if (!p) return;
      this.pending.delete(m.id);
      if (m.ok) p.resolve(m.result);
      else p.reject(new Error(m.error));
    };
  }

  static async open(appId: string, persistence: "indexeddb" | "memory", dbName: string): Promise<WorkerCore> {
    const worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module", name: `aghanim:${appId}` });
    const core = new WorkerCore(worker);
    await core.call("open", appId, persistence, dbName);
    return core;
  }

  private call<T>(method: string, ...args: unknown[]): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      this.worker.postMessage({ id, method, args });
    });
  }

  onEffects(cb: (fx: Effect[]) => void): void {
    this.listeners.push(cb);
  }

  deviceId() {
    return this.call<string>("deviceId");
  }
  userId() {
    return this.call<string | undefined>("userId");
  }
  login(alias: string, passphrase: string, remember: boolean) {
    return this.call<string>("login", alias, passphrase, remember);
  }
  loginWithSeed(seed: Uint8Array, remember: boolean) {
    return this.call<string>("loginWithSeed", seed, remember);
  }
  createRandomUser(remember: boolean) {
    return this.call<string>("createRandomUser", remember);
  }
  exportSeed() {
    return this.call<Uint8Array | undefined>("exportSeed");
  }
  logout() {
    return this.call<void>("logout");
  }
  write(writes: Uint8Array) {
    return this.call<void>("write", writes);
  }
  get(path: string) {
    return this.call<Uint8Array | undefined>("get", path);
  }
  query(query: Uint8Array) {
    return this.call<Uint8Array>("query", query);
  }
  subscribe(scope: string) {
    return this.call<void>("subscribe", scope);
  }
  connect(conn: number, sync: boolean, replicate: boolean) {
    return this.call<void>("connect", conn, sync, replicate);
  }
  disconnect(conn: number) {
    return this.call<void>("disconnect", conn);
  }
  receive(conn: number, data: Uint8Array) {
    const id = this.nextId++;
    return new Promise<void>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      this.worker.postMessage({ id, method: "receive", args: [conn, data] }, [data.buffer as ArrayBuffer]);
    });
  }
  createSpace(name: string) {
    return this.call<string>("createSpace", name);
  }
  invite(space: string, member: string, role: string) {
    return this.call<void>("invite", space, member, role);
  }
  removeMember(space: string, member: string) {
    return this.call<void>("removeMember", space, member);
  }
  members(space: string) {
    return this.call<Uint8Array>("members", space);
  }
  rawDoc(path: string) {
    return this.call<Uint8Array | undefined>("rawDoc", path);
  }
  flush() {
    return this.call<void>("flush");
  }
  async close() {
    await this.call<void>("close");
    this.worker.terminate();
  }
}
