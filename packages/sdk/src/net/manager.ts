import type { CoreApi, Effect } from "../core.ts";

export type LinkKind = "relay" | "webrtc" | "broadcast" | "custom";

/** A reliable, ordered byte pipe to one peer. */
export interface Link {
  kind: LinkKind;
  /** Human-readable endpoint (relay URL, peer id…). */
  label: string;
  send(data: Uint8Array): void;
  close(): void;
  /** Called when the sync handshake on this link completes. */
  onReady?(peer: string): void;
}

/** What a transport uses to feed its link into the engine. */
export interface LinkHandle {
  readonly conn: number;
  deliver(data: Uint8Array): void;
  closed(): void;
}

export interface LinkStatus {
  conn: number;
  kind: LinkKind;
  label: string;
  /** Authenticated device key of the peer, once the handshake is done. */
  peer?: string;
  synced: string[];
}

interface LinkState {
  link: Link;
  peer?: string;
  synced: Set<string>;
  open: boolean;
}

/**
 * Owns every connection the engine knows about, whatever its transport,
 * and routes engine effects to them.
 */
export class LinkManager {
  private links = new Map<number, LinkState>();
  private nextConn = 1;
  private syncedScopes = new Map<string, number>();
  private waiters: { scope: string; resolve: () => void }[] = [];
  onChanged: (paths: string[]) => void = () => {};

  constructor(private core: CoreApi) {
    core.onEffects((fx) => this.handle(fx));
  }

  attach(link: Link, opts: { sync: boolean; replicate: boolean }): LinkHandle {
    const conn = this.nextConn++;
    const state: LinkState = { link, synced: new Set(), open: true };
    this.links.set(conn, state);
    // Frames are delivered in order: the engine processes calls in order on
    // both the local and the worker core.
    this.core.connect(conn, opts.sync, opts.replicate).catch(report);
    return {
      conn,
      deliver: (data) => {
        if (state.open) this.core.receive(conn, data).catch(report);
      },
      closed: () => {
        if (!state.open) return;
        state.open = false;
        this.links.delete(conn);
        this.core.disconnect(conn).catch(report);
      },
    };
  }

  private handle(fx: Effect[]): void {
    for (const e of fx) {
      switch (e.type) {
        case "send":
          this.links.get(e.conn)?.link.send(e.data);
          break;
        case "changed":
          this.onChanged(e.paths);
          break;
        case "ready": {
          const s = this.links.get(e.conn);
          if (s) {
            s.peer = e.peer;
            s.link.onReady?.(e.peer);
          }
          break;
        }
        case "synced": {
          const s = this.links.get(e.conn);
          s?.synced.add(e.scope);
          this.syncedScopes.set(e.scope, (this.syncedScopes.get(e.scope) ?? 0) + 1);
          for (const w of this.waiters) if (w.scope === e.scope) w.resolve();
          this.waiters = this.waiters.filter((w) => w.scope !== e.scope);
          break;
        }
        case "close": {
          const s = this.links.get(e.conn);
          console.warn(`aghanim: closing ${s?.link.kind} link ${s?.link.label}: ${e.reason}`);
          s?.link.close();
          break;
        }
      }
    }
  }

  /** True once any peer has finished sending its backlog for `scope`. */
  isSynced(scope: string): boolean {
    return (this.syncedScopes.get(scope) ?? 0) > 0;
  }

  waitForSync(scope: string, timeoutMs: number): Promise<boolean> {
    if (this.isSynced(scope)) return Promise.resolve(true);
    return new Promise((resolve) => {
      const w = { scope, resolve: () => resolve(true) };
      this.waiters.push(w);
      setTimeout(() => {
        this.waiters = this.waiters.filter((x) => x !== w);
        resolve(false);
      }, timeoutMs);
    });
  }

  status(): LinkStatus[] {
    return [...this.links].map(([conn, s]) => ({
      conn,
      kind: s.link.kind,
      label: s.link.label,
      peer: s.peer,
      synced: [...s.synced],
    }));
  }

  closeAll(): void {
    for (const s of [...this.links.values()]) s.link.close();
  }
}

export function report(e: unknown): void {
  console.error("aghanim:", e);
}
