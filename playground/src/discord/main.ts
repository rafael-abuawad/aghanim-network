import {
  addDoc,
  collection,
  deleteDoc,
  doc,
  getDoc,
  limit,
  type Member,
  onSnapshot,
  orderBy,
  query,
  setDoc,
  type Unsubscribe,
} from "@aghanim/sdk";
import { $, colorFor, h, openApp, savedName, saveName, shortId, timeLabel } from "../shared/app.ts";
import "./discord.css";

const LOBBY = "lobby";
const LOBBY_CHANNELS = ["general", "introductions", "off-topic"];
const REQUESTS = "public/discord-lobby/requests";
const GROUP_MS = 5 * 60_000;

const db = await openApp("aghanim-playground-discord");
const me = db.auth.userId!;

/** Where a server's data lives: the public lobby scope, or an encrypted space. */
const base = (server: string) => (server === LOBBY ? "public/discord-lobby" : `spaces/${server}`);

interface ServerEntry {
  name: string;
  at: number;
  owner: boolean;
}
interface Channel {
  id: string;
  name: string;
  at: number;
}
interface JoinRequest {
  id: string;
  space: string;
  user: string;
  name: string;
  at: number;
}
type Message = {
  text: string;
  by: string;
  name: string;
  at: number;
};

const state = {
  servers: new Map<string, ServerEntry>(),
  server: "",
  serverName: "Lobby",
  channel: "",
  channels: [] as Channel[],
  members: [] as Member[],
  membersLoaded: false,
  requests: [] as JoinRequest[],
  messages: [] as Message[],
};

let serverUnsubs: Unsubscribe[] = [];
let channelUnsub: Unsubscribe | undefined;

// --- display names ---------------------------------------------------------------
// Each user publishes a profile in their own scope (`users/<pk>`), which only
// they can write; names are looked up from there.
const names = new Map<string, string>();
const watched = new Set<string>();
let myName = savedName(`anon-${me.slice(0, 4)}`);

function nameOf(pk: string, fallback?: string): string {
  return names.get(pk) || fallback || `anon-${pk.slice(0, 4)}`;
}

function watchName(pk: string): void {
  if (watched.has(pk)) return;
  watched.add(pk);
  onSnapshot(doc(db, `users/${pk}/profile/main`), (s) => {
    const n = s.get("name");
    if (typeof n !== "string" || !n.trim()) return;
    names.set(pk, n.trim().slice(0, 32));
    render();
  });
}

async function publishName(name: string): Promise<void> {
  myName = name;
  saveName(name);
  names.set(me, name);
  await setDoc(doc(db, `users/${me}/profile/main`), { name });
  render();
}

// --- routing -------------------------------------------------------------------
function route(): { server: string; channel: string; join?: string; joinName?: string } {
  const p = new URLSearchParams(location.hash.slice(1));
  return {
    server: p.get("s") || LOBBY,
    channel: p.get("c") || "general",
    join: p.get("join") ?? undefined,
    joinName: p.get("name") ?? undefined,
  };
}

function go(server: string, channel = "general"): void {
  location.hash = `s=${server}&c=${channel}`;
}

async function onRoute(): Promise<void> {
  const r = route();
  if (r.join) {
    await requestToJoin(r.join, r.joinName ?? "Server");
    go(r.join);
    return;
  }
  if (r.server !== state.server) await enterServer(r.server);
  if (r.channel !== state.channel) enterChannel(r.channel);
  render();
}

// --- servers -------------------------------------------------------------------
async function enterServer(id: string): Promise<void> {
  for (const u of serverUnsubs) u();
  channelUnsub?.();
  channelUnsub = undefined;
  Object.assign(state, {
    server: id,
    channel: "",
    channels: [],
    members: [],
    membersLoaded: false,
    messages: [],
    serverName: id === LOBBY ? "Lobby" : (state.servers.get(id)?.name ?? "Server"),
  });
  console.info(`discord: entering ${base(id)}`);

  const channels = collection(db, `${base(id)}/channels`);
  serverUnsubs = [
    onSnapshot(query(channels, orderBy("at")), (snap) => {
      state.channels = snap.docs.map((d) => ({
        id: d.id,
        name: String(d.get("name") ?? d.id),
        at: Number(d.get("at")),
      }));
      render();
    }),
  ];
  if (id === LOBBY) return;

  await db.spaces.open(id);
  serverUnsubs.push(
    onSnapshot(doc(db, `spaces/${id}/meta/info`), (s) => {
      const name = s.get("name");
      if (typeof name === "string") state.serverName = name;
      render();
    }),
    // The space document carries the member list; it changes on invites and removals.
    onSnapshot(doc(db, `spaces/${id}`), async () => {
      const members = await db.spaces.members(id).catch(() => [] as Member[]);
      if (state.server !== id) return;
      state.members = members;
      state.membersLoaded = members.length > 0;
      for (const m of members) watchName(m.pubkey);
      if (isActiveMember()) await clearOwnRequest(id);
      render();
    }),
  );
}

function isActiveMember(): boolean {
  return state.server === LOBBY || state.members.some((m) => m.pubkey === me && m.active);
}

function isOwner(server = state.server): boolean {
  return server !== LOBBY && (state.servers.get(server)?.owner ?? false);
}

async function createServer(name: string): Promise<void> {
  const space = await db.spaces.create(name);
  const at = Date.now();
  await setDoc(doc(db, `spaces/${space.id}/meta/info`), { name });
  await setDoc(doc(db, `spaces/${space.id}/channels/general`), { name: "general", at });
  await setDoc(doc(db, `users/${me}/servers/${space.id}`), { name, at, owner: true });
  console.info(`discord: created server ${space.id}`);
  go(space.id);
}

async function requestToJoin(space: string, name: string): Promise<void> {
  if (!state.servers.has(space)) {
    await setDoc(doc(db, `users/${me}/servers/${space}`), { name, at: Date.now(), owner: false });
  }
  const members = await db.spaces.members(space).catch(() => [] as Member[]);
  if (members.some((m) => m.pubkey === me && m.active)) return;
  await setDoc(doc(db, `${REQUESTS}/${space}-${me}`), { space, user: me, name: myName, at: Date.now() });
  console.info(`discord: requested to join ${space}`);
}

async function clearOwnRequest(space: string): Promise<void> {
  const ref = doc(db, `${REQUESTS}/${space}-${me}`);
  if ((await getDoc(ref)).exists()) await deleteDoc(ref);
}

async function approve(r: JoinRequest): Promise<void> {
  await db.spaces.invite(r.space, r.user, "writer");
  await deleteDoc(doc(db, `${REQUESTS}/${r.id}`));
  console.info(`discord: approved ${r.user} for ${r.space}`);
}

// --- channels and messages -----------------------------------------------------------
function enterChannel(id: string): void {
  channelUnsub?.();
  state.channel = id;
  state.messages = [];
  const messages = collection(db, `${base(state.server)}/channels/${id}/messages`);
  channelUnsub = onSnapshot(query(messages, orderBy("at", "desc"), limit(150)), (snap) => {
    state.messages = snap.docs.reverse().map((d) => ({
      text: String(d.get("text") ?? ""),
      by: String(d.get("by") ?? ""),
      name: String(d.get("name") ?? ""),
      at: Number(d.get("at")) || 0,
    }));
    for (const m of state.messages) if (m.by) watchName(m.by);
    render();
  });
}

const slug = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 24);

function visibleChannels(): Channel[] {
  const list = [...state.channels];
  if (state.server === LOBBY) {
    for (const [i, id] of LOBBY_CHANNELS.entries()) {
      if (!list.some((c) => c.id === id)) list.push({ id, name: id, at: i });
    }
  }
  return list.sort((a, b) => a.at - b.at);
}

function showError(e: unknown): void {
  const el = $("#error");
  el.textContent = e instanceof Error ? e.message : String(e);
  el.hidden = false;
  setTimeout(() => {
    el.hidden = true;
  }, 5000);
}

// --- rendering ---------------------------------------------------------------------
let renderQueued = false;
function render(): void {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    renderRail();
    renderSidebar();
    renderChannel();
    renderMembers();
  });
}

function pendingFor(server: string): JoinRequest[] {
  const active = server === state.server ? new Set(state.members.filter((m) => m.active).map((m) => m.pubkey)) : null;
  return state.requests.filter((r) => r.space === server && !active?.has(r.user));
}

function serverIcon(id: string, label: string, title: string, badge = 0): HTMLAnchorElement {
  return h(
    "a",
    {
      href: `#s=${id}&c=general`,
      class: "server",
      title,
      "aria-current": id === state.server && "true",
      style: id === LOBBY ? "" : `background:${colorFor(id)}`,
    },
    label,
    badge > 0 && h("span", { class: "badge", text: String(badge) }),
  );
}

function renderRail(): void {
  const servers = [...state.servers].sort((a, b) => a[1].at - b[1].at);
  $("#rail").replaceChildren(
    serverIcon(LOBBY, "#", "Lobby (public)"),
    h("hr"),
    ...servers.map(([id, s]) =>
      serverIcon(
        id,
        s.name
          .split(/\s+/)
          .map((w) => w[0])
          .join("")
          .slice(0, 3)
          .toUpperCase(),
        s.name,
        s.owner ? pendingFor(id).length : 0,
      ),
    ),
    h("button", {
      type: "button",
      class: "server add",
      title: "Create a server",
      "aria-label": "Create a server",
      text: "+",
      onclick: () => $<HTMLDialogElement>("#create-dialog").showModal(),
    }),
  );
}

function renderSidebar(): void {
  const lobby = state.server === LOBBY;
  $("#server-name").textContent = state.serverName;
  $("#server-kind").textContent = lobby
    ? "Public: anyone can read and write."
    : isOwner()
      ? "Encrypted space · you own it"
      : "Encrypted space";
  $("#invite").hidden = !isOwner();
  $("#leave").hidden = lobby || isOwner();
  $("#add-channel").hidden = !isActiveMember();
  $("#channels").replaceChildren(
    ...visibleChannels().map((c) =>
      h(
        "li",
        {},
        h("a", {
          href: `#s=${state.server}&c=${c.id}`,
          "aria-current": c.id === state.channel && "true",
          text: `# ${c.name}`,
        }),
      ),
    ),
  );
  $("#me-avatar").textContent = myName.slice(0, 1).toUpperCase();
  $("#me-avatar").style.background = colorFor(me);
  $("#me-id").textContent = shortId(me);
}

function renderChannel(): void {
  const gate = $("#gate");
  const list = $<HTMLOListElement>("#messages");
  const composer = $<HTMLInputElement>("#composer input");
  const channel = visibleChannels().find((c) => c.id === state.channel);
  $("#channel-name").textContent = `# ${channel?.name ?? state.channel}`;
  $("#channel-note").textContent = state.server === LOBBY ? "public" : "end-to-end encrypted";

  const member = isActiveMember();
  const removed = state.members.some((m) => m.pubkey === me && !m.active);
  gate.hidden = member;
  list.hidden = !member;
  $("#composer").hidden = !member;
  if (!member) {
    gate.replaceChildren(
      h("h2", {
        text: !state.membersLoaded ? "Connecting…" : removed ? "You were removed" : "Waiting for approval",
      }),
      h("p", {
        class: "muted",
        text: !state.membersLoaded
          ? "Fetching this server's member list from peers."
          : removed
            ? "The owner removed you and rotated the key, so new messages can't be decrypted."
            : "You asked to join. Once the owner approves, the channels appear here.",
      }),
    );
    return;
  }

  composer.placeholder = `Message #${channel?.name ?? state.channel}`;
  const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
  let prev: Message | undefined;
  list.replaceChildren(
    ...state.messages.map((m) => {
      const grouped = prev && prev.by === m.by && m.at - prev.at < GROUP_MS;
      prev = m;
      const text = h("p", { class: "text", text: m.text });
      if (grouped) return h("li", { class: "msg grouped", title: timeLabel(m.at) }, text);
      const name = nameOf(m.by, m.name);
      return h(
        "li",
        { class: "msg" },
        h("span", { class: "avatar", style: `background:${colorFor(m.by)}`, text: name.slice(0, 1).toUpperCase() }),
        h(
          "div",
          {},
          h(
            "div",
            { class: "meta" },
            h("strong", { style: `color:${colorFor(m.by)}`, text: name }),
            h("time", { class: "muted", text: timeLabel(m.at) }),
          ),
          text,
        ),
      );
    }),
  );
  if (atBottom || prev?.by === me) list.scrollTop = list.scrollHeight;
}

function memberRow(pk: string, role: string, fallbackName?: string, actions: HTMLElement[] = []): HTMLLIElement {
  const name = nameOf(pk, fallbackName);
  return h(
    "li",
    {},
    h("span", { class: "avatar small", style: `background:${colorFor(pk)}`, text: name.slice(0, 1).toUpperCase() }),
    h("span", { class: "member-name", title: pk, text: `${name}${pk === me ? " (you)" : ""}` }),
    role && h("span", { class: "role", text: role }),
    ...actions,
  );
}

function renderMembers(): void {
  const owner = isOwner();
  const pending = owner ? pendingFor(state.server) : [];
  $("#requests-box").hidden = pending.length === 0;
  $("#requests").replaceChildren(
    ...pending.map((r) =>
      memberRow(r.user, "", r.name, [
        h("button", {
          type: "button",
          class: "small",
          text: "Approve",
          onclick: () => approve(r).catch(showError),
        }),
        h("button", {
          type: "button",
          class: "ghost small",
          text: "Decline",
          onclick: () => deleteDoc(doc(db, `${REQUESTS}/${r.id}`)).catch(showError),
        }),
      ]),
    ),
  );

  if (state.server === LOBBY) {
    $("#members-title").textContent = "Recently active";
    const recent = [...new Map(state.messages.map((m) => [m.by, m])).keys()].reverse().slice(0, 30);
    $("#members").replaceChildren(...recent.map((pk) => memberRow(pk, "")));
    return;
  }
  const active = state.members.filter((m) => m.active);
  $("#members-title").textContent = `Members — ${active.length}`;
  $("#members").replaceChildren(
    ...active.map((m) =>
      memberRow(
        m.pubkey,
        m.role,
        undefined,
        owner && m.pubkey !== me
          ? [
              h("button", {
                type: "button",
                class: "ghost small",
                text: "Remove",
                title: "Remove and rotate the server key",
                onclick: () => db.spaces.remove(state.server, m.pubkey).catch(showError),
              }),
            ]
          : [],
      ),
    ),
  );
}

// --- events -----------------------------------------------------------------------
$<HTMLFormElement>("#composer").addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = $<HTMLInputElement>("#composer input");
  const text = input.value.trim();
  if (!text) return;
  input.value = "";
  const messages = collection(db, `${base(state.server)}/channels/${state.channel}/messages`);
  await addDoc(messages, { text, by: me, name: myName, at: Date.now() }).catch(showError);
});

$<HTMLFormElement>("#add-channel").addEventListener("submit", async (e) => {
  e.preventDefault();
  const form = e.target as HTMLFormElement;
  const id = slug(String(new FormData(form).get("name")));
  form.reset();
  if (!id) return;
  if (!visibleChannels().some((c) => c.id === id)) {
    await setDoc(doc(db, `${base(state.server)}/channels/${id}`), { name: id, at: Date.now(), by: me }).catch(
      showError,
    );
  }
  go(state.server, id);
});

$<HTMLFormElement>("#create-form").addEventListener("submit", (e) => {
  const form = e.target as HTMLFormElement;
  const name = String(new FormData(form).get("name")).trim();
  form.reset();
  if (name) createServer(name).catch(showError);
});

$("#invite").addEventListener("click", () => {
  const url = new URL(location.href);
  url.hash = `join=${state.server}&name=${encodeURIComponent(state.serverName)}`;
  $<HTMLInputElement>("#invite-link").value = url.toString();
  $<HTMLDialogElement>("#invite-dialog").showModal();
  $<HTMLInputElement>("#invite-link").select();
});
$("#copy-invite").addEventListener("click", async () => {
  await navigator.clipboard.writeText($<HTMLInputElement>("#invite-link").value).catch(() => {});
  $("#copy-invite").textContent = "Copied";
  setTimeout(() => {
    $("#copy-invite").textContent = "Copy link";
  }, 1500);
});

$("#leave").addEventListener("click", async () => {
  if (!confirm(`Leave ${state.serverName}? You can rejoin with an invite link.`)) return;
  const id = state.server;
  await deleteDoc(doc(db, `users/${me}/servers/${id}`)).catch(showError);
  await deleteDoc(doc(db, `${REQUESTS}/${id}-${me}`)).catch(() => {});
  go(LOBBY);
});

for (const btn of document.querySelectorAll<HTMLButtonElement>("dialog [data-close]")) {
  btn.addEventListener("click", () => btn.closest("dialog")?.close());
}

$("#me-id").addEventListener("click", () => navigator.clipboard.writeText(me).catch(() => {}));
const nameInput = $<HTMLInputElement>("#me-name");
nameInput.value = myName;
let nameTimer: ReturnType<typeof setTimeout> | undefined;
nameInput.addEventListener("input", () => {
  clearTimeout(nameTimer);
  nameTimer = setTimeout(() => {
    const name = nameInput.value.trim();
    if (name && name !== myName) publishName(name).catch(showError);
  }, 400);
});

// --- startup ----------------------------------------------------------------------
onSnapshot(collection(db, `users/${me}/servers`), (snap) => {
  state.servers = new Map(
    snap.docs.map((d) => [
      d.id,
      { name: String(d.get("name") ?? "Server"), at: Number(d.get("at")) || 0, owner: d.get("owner") === true },
    ]),
  );
  if (state.server !== LOBBY && state.servers.has(state.server) && state.serverName === "Server") {
    state.serverName = state.servers.get(state.server)!.name;
  }
  render();
});

onSnapshot(collection(db, REQUESTS), (snap) => {
  state.requests = snap.docs
    .map((d) => ({
      id: d.id,
      space: String(d.get("space") ?? ""),
      user: String(d.get("user") ?? ""),
      name: String(d.get("name") ?? "anon").slice(0, 32),
      at: Number(d.get("at")) || 0,
    }))
    .filter((r) => r.space && r.user);
  for (const r of state.requests) watchName(r.user);
  render();
});

window.addEventListener("hashchange", () => void onRoute().catch(showError));
names.set(me, myName);
if ((await getDoc(doc(db, `users/${me}/profile/main`))).get("name") !== myName) await publishName(myName);
await onRoute();
