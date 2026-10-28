import {
  addDoc,
  collection,
  deleteDoc,
  doc,
  limit,
  onSnapshot,
  openDb,
  orderBy,
  query,
  type Space,
  type Unsubscribe,
  updateDoc,
} from "@aghanim/sdk";

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;
const params = new URLSearchParams(location.search);
const relay = params.get("relay") ?? import.meta.env.VITE_RELAY ?? `ws://${location.hostname}:8787`;

const db = await openDb({ appId: "aghanim-demo", relays: [relay] });

// --- network status ---------------------------------------------------------
function showNet() {
  const s = db.network.status();
  const rtc = s.links.filter((l) => l.kind === "webrtc").length;
  const tabs = s.links.filter((l) => l.kind === "broadcast").length;
  const relays = s.relays.map((r) => r.state).join(",") || "none";
  $("#net").textContent = `relay: ${relays} · webrtc peers: ${rtc} · tabs: ${tabs}`;
}
showNet();
setInterval(showNet, 500);

// --- identity --------------------------------------------------------------
function showMe() {
  $("#me").textContent = db.auth.userId ?? "signed out";
}
showMe();
db.auth.onChange(showMe);
$("#copy-me").onclick = () => navigator.clipboard.writeText(db.auth.userId ?? "");
$<HTMLFormElement>("#login").onsubmit = async (e) => {
  e.preventDefault();
  const f = new FormData(e.target as HTMLFormElement);
  await db.auth.login(String(f.get("alias")), String(f.get("pass")));
};

// --- public chat -----------------------------------------------------------
const messages = collection(db, "public/demo/messages");
onSnapshot(query(messages, orderBy("at", "desc"), limit(50)), (snap) => {
  const ol = $("#messages");
  ol.replaceChildren(
    ...snap.docs.reverse().map((d) => {
      const li = document.createElement("li");
      const who = document.createElement("span");
      who.className = "who";
      who.textContent = String(d.get("by") ?? "").slice(0, 8);
      li.append(who, String(d.get("text") ?? ""));
      return li;
    }),
  );
  ol.scrollTop = ol.scrollHeight;
});
$<HTMLFormElement>("#say").onsubmit = async (e) => {
  e.preventDefault();
  const form = e.target as HTMLFormElement;
  const text = String(new FormData(form).get("text"));
  form.reset();
  await addDoc(messages, { text, by: db.auth.userId, at: Date.now() });
};

// --- encrypted todos -------------------------------------------------------
let unsubs: Unsubscribe[] = [];

async function enterSpace(space: Space) {
  for (const u of unsubs) u();
  history.replaceState(null, "", `#space=${space.id}`);
  $("#space-tag").textContent = `spaces/${space.id.slice(0, 8)}…`;
  $("#no-space").hidden = true;
  $("#in-space").hidden = false;
  const todos = collection(db, `spaces/${space.id}/todos`);

  unsubs = [
    onSnapshot(query(todos, orderBy("at")), (snap) => {
      $("#todos").replaceChildren(
        ...snap.docs.map((d) => {
          const li = document.createElement("li");
          li.className = d.get("done") ? "done" : "";
          const box = document.createElement("input");
          box.type = "checkbox";
          box.checked = !!d.get("done");
          box.onchange = () => updateDoc(d.ref, { done: box.checked }).catch(alert);
          const title = document.createElement("span");
          title.textContent = String(d.get("title"));
          const del = document.createElement("button");
          del.className = "ghost";
          del.textContent = "×";
          del.onclick = () => deleteDoc(d.ref).catch(alert);
          li.append(box, title, del);
          return li;
        }),
      );
    }),
    onSnapshot(doc(db, `spaces/${space.id}`), async () => {
      const members = await db.spaces.members(space).catch(() => []);
      $("#members").replaceChildren(
        ...members.map((m) => {
          const li = document.createElement("li");
          li.className = m.active ? "" : "inactive";
          li.textContent = `${m.role} ${m.pubkey.slice(0, 16)}…`;
          if (m.active && m.role !== "owner") {
            const rm = document.createElement("button");
            rm.className = "ghost";
            rm.textContent = "remove";
            rm.onclick = () => db.spaces.remove(space, m.pubkey).catch(alert);
            li.append(rm);
          }
          return li;
        }),
      );
    }),
  ];

  $<HTMLFormElement>("#add-todo").onsubmit = async (e) => {
    e.preventDefault();
    const form = e.target as HTMLFormElement;
    const title = String(new FormData(form).get("title"));
    form.reset();
    await addDoc(todos, { title, done: false, at: Date.now() }).catch(alert);
  };
  $<HTMLFormElement>("#invite").onsubmit = async (e) => {
    e.preventDefault();
    const f = new FormData(e.target as HTMLFormElement);
    await db.spaces.invite(space, String(f.get("pk")).trim(), f.get("role") as "writer" | "reader").catch(alert);
    (e.target as HTMLFormElement).reset();
  };
}

$("#create-space").onclick = async () => enterSpace(await db.spaces.create("todos"));
$("#open-space").onclick = async () => {
  const id = $<HTMLInputElement>("#space-id").value.trim();
  if (id) await enterSpace(await db.spaces.open(id));
};
async function openFromHash() {
  const id = new URLSearchParams(location.hash.slice(1)).get("space");
  if (id) await enterSpace(await db.spaces.open(id));
}
window.addEventListener("hashchange", () => void openFromHash());
await openFromHash();
