import { type LinkHandle, type LinkManager, report } from "./manager.ts";

export type SignalMessage =
  | { t: "peers"; peers: string[] }
  | { t: "joined"; peer: string }
  | { t: "left"; peer: string }
  | { t: "signal"; from: string; data: unknown }
  | { t: "error"; error: string };

export interface Signaler {
  onSignal(msg: SignalMessage, relay: RelayClient): void;
  onRelayDown(relay: RelayClient): void;
}

/** Normalizes `ws(s)://` or `http(s)://` relay URLs. */
export function relayUrls(base: string, appId: string): { ws: string; http: string } {
  const u = new URL(base);
  const secure = u.protocol === "wss:" || u.protocol === "https:";
  const root = `${u.host}${u.pathname.replace(/\/$/, "")}`;
  const app = encodeURIComponent(appId);
  return {
    ws: `${secure ? "wss" : "ws"}://${root}/v1/${app}`,
    http: `${secure ? "https" : "http"}://${root}/v1/${app}`,
  };
}

/**
 * A WebSocket to one relay, reconnecting with backoff. Binary frames are the
 * sync protocol with the relay's own node; text frames are signaling.
 */
export class RelayClient {
  private ws?: WebSocket;
  private handle?: LinkHandle;
  private stopped = true;
  private backoff = 500;
  private timer?: ReturnType<typeof setTimeout>;
  readonly urls: { ws: string; http: string };
  state: "connecting" | "open" | "closed" = "closed";

  constructor(
    base: string,
    appId: string,
    private manager: LinkManager,
    private opts: { sync: boolean; signaling: boolean },
    private signaler?: Signaler,
  ) {
    this.urls = relayUrls(base, appId);
  }

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    this.open();
  }

  stop(): void {
    this.stopped = true;
    clearTimeout(this.timer);
    this.ws?.close();
  }

  private open(): void {
    this.state = "connecting";
    const ws = new WebSocket(this.urls.ws);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.state = "open";
      this.backoff = 500;
      this.handle = this.manager.attach(
        {
          kind: "relay",
          label: this.urls.ws,
          send: (d) => ws.readyState === WebSocket.OPEN && ws.send(d),
          close: () => ws.close(),
          onReady: () => {
            if (this.opts.signaling) this.sendJson({ t: "join" });
          },
        },
        { sync: this.opts.sync, replicate: false },
      );
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        try {
          this.signaler?.onSignal(JSON.parse(ev.data) as SignalMessage, this);
        } catch (e) {
          report(e);
        }
      } else {
        this.handle?.deliver(new Uint8Array(ev.data as ArrayBuffer));
      }
    };
    ws.onclose = () => {
      this.state = "closed";
      this.handle?.closed();
      this.handle = undefined;
      this.signaler?.onRelayDown(this);
      if (!this.stopped) {
        this.timer = setTimeout(() => this.open(), this.backoff);
        this.backoff = Math.min(this.backoff * 2, 10_000);
      }
    };
    ws.onerror = () => {}; // onclose follows
  }

  sendJson(msg: unknown): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
  }

  /** STUN/TURN servers this relay recommends. */
  async iceServers(): Promise<RTCIceServer[]> {
    const r = await fetch(`${this.urls.http}/ice`);
    if (!r.ok) throw new Error(`ice config: HTTP ${r.status}`);
    return ((await r.json()) as { iceServers: RTCIceServer[] }).iceServers;
  }
}
