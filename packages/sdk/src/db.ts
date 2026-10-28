import { decode, encode } from "./cbor.ts";
import { type CoreApi, LocalCore } from "./core.ts";
import { TabMesh } from "./net/broadcast.ts";
import { type Link, type LinkHandle, LinkManager, type LinkStatus, report } from "./net/manager.ts";
import { RelayClient } from "./net/relay.ts";
import { RtcMesh } from "./net/webrtc.ts";
import { IdbPersistence, MemoryPersistence } from "./persistence.ts";

export interface OpenOptions {
  /** Separates apps sharing a relay. Peers only sync within one app id. */
  appId: string;
  /** Relay base URLs, e.g. `wss://relay.example.com`. */
  relays?: string[];
  /** Where data lives locally. Default: IndexedDB when available. */
  persistence?: "indexeddb" | "memory";
  /** Run the engine in a Web Worker. Default: true in browsers. */
  worker?: boolean;
  /** Connect directly to other browsers. Default: true when available. */
  webrtc?: boolean;
  /** Sync data with relays (false: use them for signaling only). Default: true. */
  syncWithRelays?: boolean;
  /** Sync tabs of this origin with each other. Default: true. */
  tabs?: boolean;
  /** Max simultaneous WebRTC peers. Default: 6. */
  maxPeers?: number;
  /** STUN/TURN servers. Default: asked from the first relay. */
  iceServers?: RTCIceServer[];
  /** Without a saved login, create an anonymous identity. Default: true. */
  anonymous?: boolean;
  /** Where to load the engine's .wasm from (needed outside bundlers, e.g. Node). */
  wasm?: BufferSource | URL | string;
}

interface Listener {
  matches(paths: string[]): boolean;
  refresh(): Promise<void>;
}

export type Role = "writer" | "reader";

export interface Member {
  pubkey: string;
  role: "owner" | Role;
  active: boolean;
}

export interface Space {
  id: string;
}

const hasWindow = typeof window !== "undefined";

async function acquireLeaderLock(name: string): Promise<(() => void) | null> {
  const locks = (globalThis.navigator as Navigator | undefined)?.locks;
  if (!locks) return () => {};
  return new Promise((resolve) => {
    void locks.request(name, { ifAvailable: true }, (lock) => {
      if (!lock) {
        resolve(null);
        return;
      }
      return new Promise<void>((release) => resolve(release));
    });
  });
}

/** Opens a database. One call sets up storage, identity, and networking. */
export async function openDb(options: OpenOptions): Promise<Db> {
  const o = {
    relays: [],
    persistence: typeof indexedDB !== "undefined" ? "indexeddb" : "memory",
    worker: hasWindow && typeof Worker !== "undefined",
    webrtc: typeof RTCPeerConnection !== "undefined",
    syncWithRelays: true,
    tabs: typeof BroadcastChannel !== "undefined",
    maxPeers: 6,
    anonymous: true,
    ...options,
  } satisfies OpenOptions;
  const dbName = `aghanim:${o.appId}`;

  // Only one tab may own the IndexedDB store; the others run in memory and
  // sync through it over BroadcastChannel.
  let persistence = o.persistence;
  let release: (() => void) | null = () => {};
  if (persistence === "indexeddb") {
    release = await acquireLeaderLock(dbName);
    if (!release) persistence = "memory";
  }
  const leader = persistence === "indexeddb";

  let core: CoreApi;
  if (o.worker) {
    const { WorkerCore } = await import("./worker-client.ts");
    core = await WorkerCore.open(o.appId, persistence, dbName);
  } else {
    const store = persistence === "indexeddb" ? await IdbPersistence.open(dbName) : new MemoryPersistence();
    core = await LocalCore.open(o.appId, store, o.wasm);
  }

  const db = new Db(core, o.appId, await core.deviceId(), await core.userId(), release ?? (() => {}));
  if (!db.auth.userId && o.anonymous) await db.auth.createRandom();
  await db.auth.watchOwnScope();
  db.network.configure(o.relays, {
    sync: o.syncWithRelays,
    webrtc: o.webrtc,
    maxPeers: o.maxPeers,
    iceServers: o.iceServers,
  });
  if (o.tabs) db.network.enableTabs(leader);
  db.network.goOnline();
  return db;
}

export class Db {
  readonly auth: Auth;
  readonly spaces: Spaces;
  readonly network: Network;
  /** @internal */ readonly manager: LinkManager;
  private scopes = new Set<string>();
  private listeners = new Set<Listener>();
  private dirty = new Set<Listener>();
  private flushQueued = false;
  private closed = false;

  /** @internal */
  constructor(
    /** @internal */ readonly core: CoreApi,
    readonly appId: string,
    /** This device's key, used to authenticate peer connections. */
    readonly deviceId: string,
    userId: string | undefined,
    private release: () => void,
  ) {
    this.manager = new LinkManager(core);
    this.manager.onChanged = (paths) => this.onChanged(paths);
    this.auth = new Auth(this, userId);
    this.spaces = new Spaces(this);
    this.network = new Network(this);
  }

  /** @internal Starts syncing a scope (`public/x`, `users/pk`, `spaces/id`). */
  async ensureScope(scope: string): Promise<void> {
    if (this.scopes.has(scope)) return;
    this.scopes.add(scope);
    try {
      await this.core.subscribe(scope);
    } catch (e) {
      this.scopes.delete(scope);
      throw e;
    }
  }

  /** @internal */
  addListener(l: Listener): () => void {
    this.listeners.add(l);
    this.schedule(l);
    return () => {
      this.listeners.delete(l);
      this.dirty.delete(l);
    };
  }

  /** @internal */
  refreshAll(): void {
    for (const l of this.listeners) this.schedule(l);
  }

  private onChanged(paths: string[]): void {
    for (const l of this.listeners) if (l.matches(paths)) this.schedule(l);
  }

  private schedule(l: Listener): void {
    this.dirty.add(l);
    if (this.flushQueued) return;
    this.flushQueued = true;
    // Coalesce bursts (a catch-up delivers many changes) into one refresh.
    setTimeout(() => {
      this.flushQueued = false;
      const batch = [...this.dirty];
      this.dirty.clear();
      for (const x of batch) if (this.listeners.has(x)) x.refresh().catch(report);
    }, 0);
  }

  /** Waits until local writes are durable on this device. */
  flush(): Promise<void> {
    return this.core.flush();
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.listeners.clear();
    this.network.shutdown();
    await this.core.close();
    this.release();
  }
}

export class Auth {
  private watchers = new Set<(userId: string | undefined) => void>();

  constructor(
    private db: Db,
    private current: string | undefined,
  ) {}

  /** The signed-in user's public key (hex), which is also their user id. */
  get userId(): string | undefined {
    return this.current;
  }

  private async set(userId: string | undefined): Promise<string | undefined> {
    this.current = userId;
    await this.watchOwnScope();
    this.db.refreshAll();
    for (const w of this.watchers) w(userId);
    return userId;
  }

  /** @internal */
  async watchOwnScope(): Promise<void> {
    if (this.current) await this.db.ensureScope(`users/${this.current}`);
  }

  /**
   * Signs in with an alias and passphrase. The identity is derived from them
   * (Argon2id), so the same pair signs in as the same user on any device.
   */
  async login(alias: string, passphrase: string, opts: { remember?: boolean } = {}): Promise<string> {
    return (await this.set(await this.db.core.login(alias, passphrase, opts.remember ?? true))) as string;
  }

  /** Signs in with an exported 32-byte secret seed. */
  async loginWithSeed(seed: Uint8Array, opts: { remember?: boolean } = {}): Promise<string> {
    return (await this.set(await this.db.core.loginWithSeed(seed, opts.remember ?? true))) as string;
  }

  /** Creates a new random (anonymous) identity and signs in with it. */
  async createRandom(opts: { remember?: boolean } = {}): Promise<string> {
    return (await this.set(await this.db.core.createRandomUser(opts.remember ?? true))) as string;
  }

  /** The secret seed of the signed-in user. Keep it secret. */
  exportSeed(): Promise<Uint8Array | undefined> {
    return this.db.core.exportSeed();
  }

  async logout(): Promise<void> {
    await this.db.core.logout();
    await this.set(undefined);
  }

  onChange(cb: (userId: string | undefined) => void): () => void {
    this.watchers.add(cb);
    return () => this.watchers.delete(cb);
  }
}

const spaceId = (s: Space | string) => (typeof s === "string" ? s : s.id);

export class Spaces {
  constructor(private db: Db) {}

  /** Creates an end-to-end encrypted space owned by the signed-in user. */
  async create(name: string): Promise<Space> {
    const id = await this.db.core.createSpace(name);
    await this.db.ensureScope(`spaces/${id}`);
    return { id };
  }

  /** Starts syncing a space you were invited to (or want to read). */
  async open(id: string): Promise<Space> {
    await this.db.ensureScope(`spaces/${id}`);
    return { id };
  }

  /** Adds a member by public key. Owner only. */
  invite(space: Space | string, pubkey: string, role: Role = "writer"): Promise<void> {
    return this.db.core.invite(spaceId(space), pubkey, role);
  }

  /** Removes a member and rotates the space key. Owner only. */
  remove(space: Space | string, pubkey: string): Promise<void> {
    return this.db.core.removeMember(spaceId(space), pubkey);
  }

  async members(space: Space | string): Promise<Member[]> {
    const rows = decode<[string, Member["role"], boolean][]>(await this.db.core.members(spaceId(space)));
    return rows.map(([pubkey, role, active]) => ({ pubkey, role, active }));
  }
}

export interface NetworkStatus {
  deviceId: string;
  online: boolean;
  relays: { url: string; state: RelayClient["state"] }[];
  links: LinkStatus[];
}

export class Network {
  private relays: RelayClient[] = [];
  private mesh?: RtcMesh;
  private tabs?: TabMesh;
  private online = false;

  constructor(private db: Db) {}

  /** @internal */
  configure(
    urls: string[],
    o: { sync: boolean; webrtc: boolean; maxPeers: number; iceServers?: RTCIceServer[] },
  ): void {
    if (o.webrtc) this.mesh = new RtcMesh(this.db.manager, this.db.deviceId, o.maxPeers, o.iceServers);
    this.relays = urls.map(
      (u) => new RelayClient(u, this.db.appId, this.db.manager, { sync: o.sync, signaling: o.webrtc }, this.mesh),
    );
  }

  /** @internal */
  enableTabs(leader: boolean): void {
    this.tabs = new TabMesh(this.db.appId, this.db.manager, leader);
  }

  /** Connects to relays (and, through them, to peers). */
  goOnline(): void {
    this.online = true;
    for (const r of this.relays) r.start();
  }

  /** Drops every relay and peer connection. Local reads and writes keep working. */
  goOffline(): void {
    this.online = false;
    for (const r of this.relays) r.stop();
    this.mesh?.closeAll();
  }

  /** Attaches a custom transport (any reliable, ordered byte pipe). */
  addLink(link: Omit<Link, "kind"> & { kind?: Link["kind"] }, opts: { replicate?: boolean } = {}): LinkHandle {
    return this.db.manager.attach({ kind: "custom", ...link }, { sync: true, replicate: opts.replicate ?? false });
  }

  /**
   * Resolves true once some peer has sent its backlog for the scope of
   * `target` (a path or scope), or false after `timeoutMs`.
   */
  waitForSync(target: string | { path: string }, timeoutMs = 5000): Promise<boolean> {
    const path = typeof target === "string" ? target : target.path;
    const scope = path.split("/").slice(0, 2).join("/");
    return this.db.manager.waitForSync(scope, timeoutMs);
  }

  status(): NetworkStatus {
    return {
      deviceId: this.db.deviceId,
      online: this.online,
      relays: this.relays.map((r) => ({ url: r.urls.ws, state: r.state })),
      links: this.db.manager.status(),
    };
  }

  /** @internal */
  shutdown(): void {
    this.goOffline();
    this.tabs?.close();
    this.db.manager.closeAll();
  }
}

/** @internal Decodes a raw stored document, for debugging. */
export async function rawDoc(db: Db, path: string): Promise<unknown> {
  const b = await db.core.rawDoc(path);
  return b ? decode(b) : undefined;
}

export { decode as decodeCbor, encode as encodeCbor };
