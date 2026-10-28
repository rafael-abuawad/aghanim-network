import type { LinkHandle, LinkManager } from "./manager.ts";

type Envelope =
  | { t: "hello"; from: string }
  | { t: "ack"; from: string; to: string }
  | { t: "data"; from: string; to: string; data: Uint8Array }
  | { t: "bye"; from: string };

/**
 * Syncs tabs of the same origin over a BroadcastChannel, pairwise. The tab
 * holding the database lock replicates for the others, so their writes are
 * persisted even though they run on an in-memory store.
 */
export class TabMesh {
  private bc: BroadcastChannel;
  private id = crypto.randomUUID();
  private links = new Map<string, LinkHandle>();

  constructor(
    appId: string,
    private manager: LinkManager,
    private replicate: boolean,
  ) {
    this.bc = new BroadcastChannel(`aghanim:${appId}`);
    this.bc.onmessage = (ev: MessageEvent<Envelope>) => this.onMessage(ev.data);
    this.post({ t: "hello", from: this.id });
  }

  private post(m: Envelope): void {
    this.bc.postMessage(m);
  }

  private link(other: string): void {
    if (this.links.has(other)) return;
    const handle = this.manager.attach(
      {
        kind: "broadcast",
        label: `tab:${other.slice(0, 8)}`,
        send: (data) => this.post({ t: "data", from: this.id, to: other, data }),
        close: () => {
          this.links.get(other)?.closed();
          this.links.delete(other);
        },
      },
      { sync: true, replicate: this.replicate },
    );
    this.links.set(other, handle);
  }

  private onMessage(m: Envelope): void {
    if (m.from === this.id) return;
    switch (m.t) {
      case "hello":
        this.post({ t: "ack", from: this.id, to: m.from });
        this.link(m.from);
        break;
      case "ack":
        if (m.to === this.id) this.link(m.from);
        break;
      case "data":
        if (m.to === this.id) this.links.get(m.from)?.deliver(m.data);
        break;
      case "bye":
        this.links.get(m.from)?.closed();
        this.links.delete(m.from);
        break;
    }
  }

  close(): void {
    this.post({ t: "bye", from: this.id });
    for (const h of this.links.values()) h.closed();
    this.links.clear();
    this.bc.close();
  }
}
