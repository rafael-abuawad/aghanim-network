import { type Db, type LinkHandle, openDb } from "@aghanim/sdk";

/** The in-memory connection between the two peers, which scripts can cut. */
export interface LinkControl {
  readonly up: boolean;
  cut(): void;
  restore(): void;
}

export interface Peers {
  alice: Db;
  bob: Db;
  link: LinkControl;
  close(): Promise<void>;
}

// Each peer gets its own engine Worker and in-memory store, and no relays,
// WebRTC or cross-tab sync: the only way data moves is the link below.
function openPeer(): Promise<Db> {
  return openDb({ appId: "aghanim-playground", persistence: "memory", relays: [], webrtc: false, tabs: false });
}

/**
 * Connects two databases with an ordered in-memory pipe. Returns a function
 * that disconnects it; `onDown` runs once however the link goes down.
 */
function connect(a: Db, b: Db, onDown: () => void): () => void {
  let up = true;
  let ha: LinkHandle | undefined;
  let hb: LinkHandle | undefined;
  const cut = () => {
    if (!up) return;
    up = false;
    ha?.closed();
    hb?.closed();
    onDown();
  };
  ha = a.network.addLink({
    label: "bob",
    send: (d) => up && queueMicrotask(() => hb?.deliver(d.slice())),
    close: cut,
  });
  hb = b.network.addLink({
    label: "alice",
    send: (d) => up && queueMicrotask(() => ha?.deliver(d.slice())),
    close: cut,
  });
  return cut;
}

export async function createPeers(onLinkChange: (up: boolean) => void): Promise<Peers> {
  const [alice, bob] = await Promise.all([openPeer(), openPeer()]);
  let disconnect: (() => void) | undefined;
  let closing = false;

  const link: LinkControl = {
    get up() {
      return disconnect !== undefined;
    },
    cut() {
      disconnect?.();
    },
    restore() {
      if (disconnect || closing) return;
      disconnect = connect(alice, bob, () => {
        disconnect = undefined;
        if (!closing) onLinkChange(false);
      });
      onLinkChange(true);
    },
  };
  link.restore();

  return {
    alice,
    bob,
    link,
    async close() {
      closing = true;
      disconnect?.();
      await Promise.all([alice.close(), bob.close()]);
    },
  };
}
