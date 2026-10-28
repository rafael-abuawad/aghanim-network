import { type LinkHandle, type LinkManager, report } from "./manager.ts";
import type { RelayClient, Signaler, SignalMessage } from "./relay.ts";

/** DataChannel frames carry at most this much (a 1-byte header + payload). */
export const CHUNK = 16 * 1024;
const HIGH_WATER = 1024 * 1024;

/** Splits a frame into chunks: header byte 1 = more follows, 0 = last. */
export function chunk(data: Uint8Array, size = CHUNK): Uint8Array[] {
  const body = size - 1;
  const out: Uint8Array[] = [];
  for (let off = 0; off < data.length || out.length === 0; off += body) {
    const part = data.subarray(off, off + body);
    const c = new Uint8Array(part.length + 1);
    c[0] = off + body < data.length ? 1 : 0;
    c.set(part, 1);
    out.push(c);
  }
  return out;
}

export class Reassembler {
  private parts: Uint8Array[] = [];
  /** Returns the complete frame when `c` is its last chunk. */
  push(c: Uint8Array): Uint8Array | undefined {
    this.parts.push(c.subarray(1));
    if (c[0] === 1) return undefined;
    const len = this.parts.reduce((n, p) => n + p.length, 0);
    const out = new Uint8Array(len);
    let off = 0;
    for (const p of this.parts) {
      out.set(p, off);
      off += p.length;
    }
    this.parts = [];
    return out;
  }
}

type Signal = { sdp?: RTCSessionDescriptionInit; candidate?: RTCIceCandidateInit };

class PeerConn {
  readonly pc: RTCPeerConnection;
  private dc: RTCDataChannel;
  private handle?: LinkHandle;
  private queue: Uint8Array[] = [];
  private pendingIce: RTCIceCandidateInit[] = [];
  private reasm = new Reassembler();
  closed = false;

  constructor(
    private mesh: RtcMesh,
    readonly peer: string,
    readonly via: RelayClient,
    iceServers: RTCIceServer[],
    initiator: boolean,
  ) {
    this.pc = new RTCPeerConnection({ iceServers });
    // Negotiated with a fixed id, so neither side waits for `ondatachannel`.
    this.dc = this.pc.createDataChannel("aghanim", { negotiated: true, id: 0, ordered: true });
    this.dc.binaryType = "arraybuffer";
    this.dc.bufferedAmountLowThreshold = HIGH_WATER / 4;
    this.dc.onopen = () => {
      this.handle = mesh.manager.attach(
        { kind: "webrtc", label: peer, send: (d) => this.send(d), close: () => this.close() },
        { sync: true, replicate: false },
      );
    };
    this.dc.onmessage = (ev) => {
      const frame = this.reasm.push(new Uint8Array(ev.data as ArrayBuffer));
      if (frame) this.handle?.deliver(frame);
    };
    this.dc.onbufferedamountlow = () => this.drain();
    this.dc.onclose = () => this.close();
    this.pc.onicecandidate = (ev) => {
      if (ev.candidate) this.signal({ candidate: ev.candidate.toJSON() });
    };
    this.pc.onconnectionstatechange = () => {
      if (this.pc.connectionState === "failed" || this.pc.connectionState === "closed") this.close();
    };
    if (initiator) {
      this.pc
        .createOffer()
        .then((o) => this.pc.setLocalDescription(o))
        .then(() => this.signal({ sdp: this.pc.localDescription!.toJSON() }))
        .catch((e) => this.fail(e));
    }
  }

  private signal(data: Signal): void {
    this.via.sendJson({ t: "signal", to: this.peer, data });
  }

  async onSignal(data: Signal): Promise<void> {
    try {
      if (data.sdp) {
        await this.pc.setRemoteDescription(data.sdp);
        for (const c of this.pendingIce.splice(0)) await this.pc.addIceCandidate(c);
        if (data.sdp.type === "offer") {
          await this.pc.setLocalDescription(await this.pc.createAnswer());
          this.signal({ sdp: this.pc.localDescription!.toJSON() });
        }
      } else if (data.candidate) {
        if (this.pc.remoteDescription) await this.pc.addIceCandidate(data.candidate);
        else this.pendingIce.push(data.candidate);
      }
    } catch (e) {
      this.fail(e);
    }
  }

  private send(data: Uint8Array): void {
    for (const c of chunk(data)) this.queue.push(c);
    this.drain();
  }

  private drain(): void {
    while (this.queue.length > 0 && this.dc.readyState === "open" && this.dc.bufferedAmount < HIGH_WATER) {
      this.dc.send(this.queue.shift() as Uint8Array<ArrayBuffer>);
    }
  }

  private fail(e: unknown): void {
    report(e);
    this.close();
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.handle?.closed();
    this.dc.close();
    this.pc.close();
    this.mesh.forget(this);
  }
}

/**
 * Browser-to-browser mesh. Relays introduce peers; the peer with the lower
 * device id makes the offer, so two peers never both offer.
 */
export class RtcMesh implements Signaler {
  private peers = new Map<string, PeerConn>();
  private ice?: Promise<RTCIceServer[]>;

  constructor(
    readonly manager: LinkManager,
    private myId: string,
    private maxPeers: number,
    private iceServers?: RTCIceServer[],
  ) {}

  private servers(via: RelayClient): Promise<RTCIceServer[]> {
    if (this.iceServers) return Promise.resolve(this.iceServers);
    this.ice ??= via.iceServers().catch((e) => {
      this.ice = undefined;
      report(e);
      return [];
    });
    return this.ice;
  }

  onSignal(msg: SignalMessage, via: RelayClient): void {
    switch (msg.t) {
      case "peers":
        for (const p of msg.peers) this.maybeDial(p, via);
        break;
      case "joined":
        this.maybeDial(msg.peer, via);
        break;
      case "left":
        this.peers.get(msg.peer)?.close();
        break;
      case "signal": {
        const data = msg.data as Signal;
        let pc = this.peers.get(msg.from);
        if (!pc && data.sdp?.type === "offer" && this.peers.size < this.maxPeers) {
          void this.servers(via).then((ice) => {
            if (this.peers.has(msg.from)) return void this.peers.get(msg.from)?.onSignal(data);
            pc = new PeerConn(this, msg.from, via, ice, false);
            this.peers.set(msg.from, pc);
            void pc.onSignal(data);
          });
          return;
        }
        void pc?.onSignal(data);
        break;
      }
      case "error":
        console.warn("aghanim relay:", msg.error);
        break;
    }
  }

  onRelayDown(via: RelayClient): void {
    // Established DataChannels survive their relay; only unfinished
    // negotiations depended on it.
    for (const p of [...this.peers.values()]) {
      if (p.via === via && p.pc.connectionState !== "connected") p.close();
    }
  }

  private maybeDial(peer: string, via: RelayClient): void {
    if (peer === this.myId || this.peers.has(peer) || this.peers.size >= this.maxPeers) return;
    if (this.myId > peer) return; // they dial us
    void this.servers(via).then((ice) => {
      if (this.peers.has(peer)) return;
      this.peers.set(peer, new PeerConn(this, peer, via, ice, true));
    });
  }

  forget(p: PeerConn): void {
    if (this.peers.get(p.peer) === p) this.peers.delete(p.peer);
  }

  closeAll(): void {
    for (const p of [...this.peers.values()]) p.close();
  }

  get size(): number {
    return this.peers.size;
  }
}
