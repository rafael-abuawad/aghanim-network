import { type Db, type OpenOptions, openDb } from "@aghanim/sdk";
import "./base.css";

export const $ = <T extends HTMLElement>(sel: string, root: ParentNode = document) => root.querySelector(sel) as T;

/** The relay to use: `?relay=` in the URL, `VITE_RELAY`, or port 8787 on this host. */
export function relayUrl(): string {
  return (
    new URLSearchParams(location.search).get("relay") ?? import.meta.env.VITE_RELAY ?? `ws://${location.hostname}:8787`
  );
}

/** Opens the app's database against the configured relay and shows its status in the top bar. */
export async function openApp(appId: string, extra: Partial<OpenOptions> = {}): Promise<Db> {
  const db = await openDb({ appId, relays: [relayUrl()], ...extra });
  showNetwork(db);
  return db;
}

function showNetwork(db: Db): void {
  const el = document.querySelector<HTMLElement>("#net");
  if (!el) return;
  const update = () => {
    const s = db.network.status();
    const relay = s.relays[0]?.state ?? "closed";
    const peers = s.links.filter((l) => l.kind === "webrtc" && l.peer).length;
    const tabs = s.links.filter((l) => l.kind === "broadcast").length;
    el.dataset.state = relay;
    el.textContent = `relay ${relay} · ${peers} peer${peers === 1 ? "" : "s"} · ${tabs} tab${tabs === 1 ? "" : "s"}`;
    el.title = `Relay: ${s.relays[0]?.url ?? "none"}\nWebRTC peers connect directly; tabs sync over BroadcastChannel.`;
  };
  update();
  setInterval(update, 1000);
}

/** Highlights the current page in the top bar's app links. */
export function markNav(): void {
  for (const a of document.querySelectorAll<HTMLAnchorElement>(".apps a")) {
    const current = new URL(a.href).pathname === location.pathname;
    if (current) a.setAttribute("aria-current", "page");
  }
}
markNav();

type Child = Node | string | null | undefined | false;

/** Small DOM builder: `h("li", { class: "msg" }, "text", child)`. */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<Record<string, string | boolean | ((e: Event) => void)>> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === false) continue;
    if (typeof v === "function") el.addEventListener(k.replace(/^on/, ""), v);
    else if (k === "class") el.className = String(v);
    else if (k === "text") el.textContent = String(v);
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children) if (c) el.append(c);
  return el;
}

/** A stable, readable color for an id. */
export function colorFor(id: string): string {
  let hash = 0;
  for (const ch of id) hash = (hash * 31 + ch.charCodeAt(0)) | 0;
  return `hsl(${Math.abs(hash) % 360} 65% 58%)`;
}

export const shortId = (id: string) => `${id.slice(0, 6)}…${id.slice(-4)}`;

const NAME_KEY = "aghanim-playground:name";

/** The display name this browser uses across the demo apps. */
export function savedName(fallback: string): string {
  return localStorage.getItem(NAME_KEY) || fallback;
}

export function saveName(name: string): void {
  localStorage.setItem(NAME_KEY, name);
}

export function timeLabel(at: number): string {
  return new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
