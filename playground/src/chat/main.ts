import {
  addDoc,
  collection,
  doc,
  limit,
  onSnapshot,
  orderBy,
  type QueryDocumentSnapshot,
  query,
  setDoc,
  type Unsubscribe,
} from "@aghanim/sdk";
import { $, colorFor, h, openApp, savedName, saveName, timeLabel } from "../shared/app.ts";
import "./chat.css";

const ONLINE_MS = 30_000;
const HEARTBEAT_MS = 10_000;
const TYPING_MS = 3_000;
const GROUP_MS = 5 * 60_000;
const ROOMS_KEY = "aghanim-playground:chat-rooms";

const db = await openApp("aghanim-playground-chat");
const me = db.auth.userId!;

// --- name -----------------------------------------------------------------------
const nameInput = $<HTMLInputElement>("#name");
let myName = savedName(`anon-${me.slice(0, 4)}`);
nameInput.value = myName;
let nameTimer: ReturnType<typeof setTimeout> | undefined;
nameInput.addEventListener("input", () => {
  clearTimeout(nameTimer);
  nameTimer = setTimeout(() => {
    const name = nameInput.value.trim();
    if (!name || name === myName) return;
    myName = name;
    saveName(myName);
    void beat();
  }, 400);
});

// --- rooms ----------------------------------------------------------------------
const cleanRoom = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 32);

function knownRooms(): string[] {
  const saved = JSON.parse(localStorage.getItem(ROOMS_KEY) ?? "[]") as string[];
  return [...new Set(["general", "random", ...saved])];
}

function rememberRoom(room: string): void {
  const rooms = knownRooms().filter((r) => r !== room);
  localStorage.setItem(ROOMS_KEY, JSON.stringify([room, ...rooms].slice(0, 12)));
}

function roomFromHash(): string {
  return cleanRoom(new URLSearchParams(location.hash.slice(1)).get("room") ?? "") || "general";
}

function renderRooms(current: string): void {
  $("#rooms").replaceChildren(
    ...knownRooms().map((r) =>
      h("li", {}, h("a", { href: `#room=${r}`, "aria-current": r === current && "true", text: `# ${r}` })),
    ),
  );
}

$<HTMLFormElement>("#join").addEventListener("submit", (e) => {
  e.preventDefault();
  const form = e.target as HTMLFormElement;
  const room = cleanRoom(String(new FormData(form).get("room")));
  form.reset();
  if (room) location.hash = `room=${room}`;
});

// --- the current room -----------------------------------------------------------
type Message = {
  text: string;
  by: string;
  name: string;
  at: number;
};
interface Presence {
  name: string;
  at: number;
  typing: boolean;
}

let room = "";
let unsubs: Unsubscribe[] = [];
let presence = new Map<string, Presence>();
let typingSince = 0;
let typingTimer: ReturnType<typeof setTimeout> | undefined;

const scopeOf = (r: string) => `public/chat-${r}`;
const myPresence = () => doc(db, `${scopeOf(room)}/presence/${me}`);

async function beat(typing = false): Promise<void> {
  if (!room) return;
  await setDoc(myPresence(), { name: myName, at: Date.now(), typing });
}

function enter(next: string): void {
  if (next === room) return;
  // Mark ourselves offline in the room we're leaving.
  if (room) void setDoc(myPresence(), { at: 0, typing: false }, { merge: true });
  for (const u of unsubs) u();
  room = next;
  presence = new Map();
  rememberRoom(room);
  renderRooms(room);
  $("#room-title").textContent = `# ${room}`;
  $("#room-scope").textContent = scopeOf(room);
  $<HTMLInputElement>("#composer input").placeholder = `Message #${room}`;
  $("#messages").replaceChildren();
  console.info(`chat: entering ${scopeOf(room)}`);

  const messages = collection(db, `${scopeOf(room)}/messages`);
  unsubs = [
    onSnapshot(query(messages, orderBy("at", "desc"), limit(200)), (snap) => {
      renderMessages(snap.docs.slice().reverse() as QueryDocumentSnapshot<Message>[]);
    }),
    onSnapshot(collection(db, `${scopeOf(room)}/presence`), (snap) => {
      presence = new Map(
        snap.docs.map((d) => [
          d.id,
          { name: String(d.get("name") || "anon"), at: Number(d.get("at")) || 0, typing: d.get("typing") === true },
        ]),
      );
      renderPresence();
    }),
  ];
  void beat();
}

function renderMessages(docs: QueryDocumentSnapshot<Message>[]): void {
  const list = $<HTMLOListElement>("#messages");
  const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
  let prev: Message | undefined;
  list.replaceChildren(
    ...docs.map((d) => {
      // Anyone can write to public/, so don't trust field types.
      const m = { ...d.data(), name: String(d.data().name || "anon"), by: String(d.data().by ?? "") };
      const grouped = prev && prev.by === m.by && m.at - prev.at < GROUP_MS;
      prev = m;
      const text = h("p", { class: "text", text: String(m.text) });
      if (grouped) return h("li", { class: "msg grouped", title: timeLabel(m.at) }, text);
      return h(
        "li",
        { class: `msg${m.by === me ? " mine" : ""}` },
        h("span", { class: "avatar", style: `background:${colorFor(m.by)}`, text: m.name.slice(0, 1).toUpperCase() }),
        h(
          "div",
          {},
          h(
            "div",
            { class: "meta" },
            h("strong", { style: `color:${colorFor(m.by)}`, text: m.name }),
            h("time", { class: "muted", text: timeLabel(m.at) }),
          ),
          text,
        ),
      );
    }),
  );
  if (atBottom || prev?.by === me) list.scrollTop = list.scrollHeight;
}

function renderPresence(): void {
  const now = Date.now();
  const online = [...presence]
    .filter(([, p]) => now - p.at < ONLINE_MS)
    .sort((a, b) => a[1].name.localeCompare(b[1].name));
  $("#online-count").textContent = String(online.length);
  $("#online").replaceChildren(
    ...online.map(([id, p]) =>
      h(
        "li",
        {},
        h("span", { class: "dot", style: `background:${colorFor(id)}` }),
        `${p.name}${id === me ? " (you)" : ""}`,
      ),
    ),
  );
  const typing = online.filter(([id, p]) => p.typing && id !== me).map(([, p]) => p.name);
  $("#typing").textContent =
    typing.length === 0 ? "" : typing.length === 1 ? `${typing[0]} is typing…` : `${typing.join(", ")} are typing…`;
}

// --- composing ------------------------------------------------------------------
const composer = $<HTMLFormElement>("#composer");
const textInput = $<HTMLInputElement>("#composer input");

textInput.addEventListener("input", () => {
  if (!textInput.value) return;
  if (Date.now() - typingSince > TYPING_MS - 500) {
    typingSince = Date.now();
    void beat(true);
  }
  clearTimeout(typingTimer);
  typingTimer = setTimeout(() => {
    typingSince = 0;
    void beat(false);
  }, TYPING_MS);
});

composer.addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = textInput.value.trim();
  if (!text) return;
  composer.reset();
  clearTimeout(typingTimer);
  typingSince = 0;
  await addDoc(collection(db, `${scopeOf(room)}/messages`), { text, by: me, name: myName, at: Date.now() });
  void beat(false);
});

window.addEventListener("hashchange", () => enter(roomFromHash()));
window.addEventListener("pagehide", () => void setDoc(myPresence(), { at: 0, typing: false }, { merge: true }));
setInterval(() => void beat(typingSince > 0), HEARTBEAT_MS);
setInterval(renderPresence, 5_000);

enter(roomFromHash());
textInput.focus();
