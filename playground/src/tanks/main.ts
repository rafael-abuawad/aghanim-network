import {
  addDoc,
  collection,
  deleteDoc,
  doc,
  limit,
  newId,
  onSnapshot,
  orderBy,
  query,
  setDoc,
  type Unsubscribe,
} from "@aghanim/sdk";
import { $, colorFor, h, openApp, savedName, saveName } from "../shared/app.ts";
import {
  blocked,
  FIRE_COOLDOWN_MS,
  HEIGHT,
  HIT_DAMAGE,
  lerpAngle,
  move,
  RESPAWN_MS,
  randomSpawn,
  SHOT_RADIUS,
  SHOT_SPEED,
  shotLifetime,
  TANK_RADIUS,
  TANK_SPEED,
  TURN_SPEED,
  WALLS,
  WIDTH,
} from "./world.ts";
import "./tanks.css";

const SEND_EVERY_MS = 80;
const HEARTBEAT_MS = 2000;
const STALE_MS = 6000;
// Tolerated clock difference between peers when judging if a record is current.
const SKEW_MS = 8000;

// In-memory storage gives every tab its own identity (and so its own tank),
// and keeps the high-rate position updates out of IndexedDB.
const db = await openApp("aghanim-playground-tanks", { persistence: "memory" });
const me = db.auth.userId!;
const myColor = colorFor(me);

interface RemoteTank {
  name: string;
  color: string;
  hp: number;
  alive: boolean;
  x: number;
  y: number;
  a: number;
  t: number;
  target: { x: number; y: number; a: number; t: number };
  seen: number;
}
interface Shot {
  owner: string;
  x: number;
  y: number;
  vx: number;
  vy: number;
  born: number;
  life: number;
}
interface Kill {
  killer: string;
  victim: string;
  killerName: string;
  victimName: string;
}
interface Blast {
  x: number;
  y: number;
  color: string;
  born: number;
  big: boolean;
}

const spawn = randomSpawn();
const self = {
  name: savedName(`anon-${me.slice(0, 4)}`),
  ...spawn,
  t: spawn.a,
  hp: 100,
  alive: true,
  respawnAt: 0,
  lastFire: 0,
};
const tanks = new Map<string, RemoteTank>();
const shots = new Map<string, Shot>();
const spent = new Set<string>();
const blasts: Blast[] = [];
let kills: Kill[] = [];

// --- room ----------------------------------------------------------------------------
const cleanRoom = (s: string) =>
  s
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 24);
let room = "";
let unsubs: Unsubscribe[] = [];
const scope = () => `public/tanks-${room}`;
const myDoc = () => doc(db, `${scope()}/players/${me}`);

function enterRoom(next: string): void {
  if (next === room) return;
  if (room) void setDoc(myDoc(), { alive: false, at: 0 }, { merge: true });
  for (const u of unsubs) u();
  room = next;
  tanks.clear();
  shots.clear();
  kills = [];
  $<HTMLInputElement>("#room").value = room;
  console.info(`tanks: joining ${scope()}`);

  unsubs = [
    onSnapshot(collection(db, `${scope()}/players`), (snap) => {
      const now = performance.now();
      for (const c of snap.docChanges()) {
        const id = c.doc.id;
        if (id === me) continue;
        if (c.type === "removed") {
          tanks.delete(id);
          continue;
        }
        const d = c.doc.data();
        if (Math.abs(Date.now() - Number(d.at)) > SKEW_MS) continue;
        const target = { x: Number(d.x), y: Number(d.y), a: Number(d.a), t: Number(d.t) };
        if (!Object.values(target).every(Number.isFinite)) continue;
        const known = tanks.get(id);
        const wasAlive = known?.alive ?? d.alive === true;
        const tank: RemoteTank = known ?? {
          ...target,
          target,
          name: "",
          color: colorFor(id),
          hp: 100,
          alive: true,
          seen: 0,
        };
        tank.target = target;
        tank.name = String(d.name ?? "anon").slice(0, 24);
        tank.hp = Number(d.hp) || 0;
        tank.alive = d.alive === true;
        tank.seen = now;
        // Snap instead of gliding across the map after a respawn.
        if (!wasAlive && tank.alive) Object.assign(tank, target);
        if (wasAlive && !tank.alive) blasts.push({ x: tank.x, y: tank.y, color: tank.color, born: now, big: true });
        tanks.set(id, tank);
      }
    }),
    onSnapshot(collection(db, `${scope()}/shots`), (snap) => {
      for (const c of snap.docChanges()) {
        if (c.type !== "added" || shots.has(c.doc.id)) continue;
        const d = c.doc.data();
        const age = Date.now() - Number(d.at);
        if (!(Math.abs(age) < SKEW_MS) || d.owner === me) continue;
        const [x, y, vx, vy] = [d.x, d.y, d.vx, d.vy].map(Number);
        if (![x, y, vx, vy].every(Number.isFinite) || Math.hypot(vx, vy) > SHOT_SPEED * 1.01) continue;
        // Start the shot a little "into" its flight to make up for network delay.
        const head = Math.min(Math.max(age, 0), 150);
        shots.set(c.doc.id, {
          owner: String(d.owner),
          x,
          y,
          vx,
          vy,
          born: performance.now() - head,
          life: shotLifetime(x, y, vx, vy),
        });
      }
    }),
    onSnapshot(query(collection(db, `${scope()}/kills`), orderBy("at", "desc"), limit(300)), (snap) => {
      kills = snap.docs.map((d) => ({
        killer: String(d.get("killer") ?? ""),
        victim: String(d.get("victim") ?? ""),
        killerName: String(d.get("killerName") ?? "anon").slice(0, 24),
        victimName: String(d.get("victimName") ?? "anon").slice(0, 24),
      }));
      renderHud();
    }),
  ];
  void publish(true);
}

// --- publishing our tank -----------------------------------------------------------------
let lastSent = 0;
let lastSentKey = "";
let sending = false;

async function publish(force = false): Promise<void> {
  const now = performance.now();
  const state = {
    name: self.name,
    x: Math.round(self.x * 10) / 10,
    y: Math.round(self.y * 10) / 10,
    a: Math.round(self.a * 1000) / 1000,
    t: Math.round(self.t * 1000) / 1000,
    hp: self.hp,
    alive: self.alive,
  };
  const key = JSON.stringify(state);
  const due = force || (key !== lastSentKey && now - lastSent >= SEND_EVERY_MS) || now - lastSent >= HEARTBEAT_MS;
  if (!due || sending || !room) return;
  sending = true;
  lastSent = now;
  lastSentKey = key;
  try {
    await setDoc(myDoc(), { ...state, at: Date.now() });
  } catch (e) {
    console.error("tanks: publishing state failed", e);
  } finally {
    sending = false;
  }
}

function fire(): void {
  const now = performance.now();
  if (!self.alive || now - self.lastFire < FIRE_COOLDOWN_MS) return;
  self.lastFire = now;
  const muzzle = TANK_RADIUS + 10;
  const x = self.x + Math.cos(self.t) * muzzle;
  const y = self.y + Math.sin(self.t) * muzzle;
  if (blocked(x, y, SHOT_RADIUS)) return;
  const vx = Math.cos(self.t) * SHOT_SPEED;
  const vy = Math.sin(self.t) * SHOT_SPEED;
  const id = newId();
  const life = shotLifetime(x, y, vx, vy);
  shots.set(id, { owner: me, x, y, vx, vy, born: now, life });
  const ref = doc(db, `${scope()}/shots/${id}`);
  setDoc(ref, { owner: me, x, y, vx, vy, at: Date.now() })
    .then(() => setTimeout(() => void deleteDoc(ref).catch(() => {}), life + 500))
    .catch((e) => console.error("tanks: firing failed", e));
}

// Whoever gets hit decides it, so each peer only ever writes its own state.
function takeHit(shotId: string, shot: Shot, now: number): void {
  spent.add(shotId);
  self.hp = Math.max(0, self.hp - HIT_DAMAGE);
  blasts.push({ x: self.x, y: self.y, color: myColor, born: now, big: false });
  if (self.hp === 0) {
    self.alive = false;
    self.respawnAt = now + RESPAWN_MS;
    blasts.push({ x: self.x, y: self.y, color: myColor, born: now, big: true });
    const killerName = tanks.get(shot.owner)?.name ?? "someone";
    void addDoc(collection(db, `${scope()}/kills`), {
      killer: shot.owner,
      victim: me,
      killerName,
      victimName: self.name,
      at: Date.now(),
    });
  }
  void publish(true);
}

// --- input ---------------------------------------------------------------------------
const keys = new Set<string>();
let mouseDown = false;
const canvas = $<HTMLCanvasElement>("#arena");
const ctx = canvas.getContext("2d")!;

const typing = (e: Event) => e.target instanceof HTMLInputElement;
window.addEventListener("keydown", (e) => {
  if (typing(e)) return;
  keys.add(e.code);
  if (["Space", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"].includes(e.code)) e.preventDefault();
});
window.addEventListener("keyup", (e) => keys.delete(e.code));
window.addEventListener("blur", () => keys.clear());

function toWorld(e: MouseEvent): { x: number; y: number } {
  const r = canvas.getBoundingClientRect();
  return { x: ((e.clientX - r.left) / r.width) * WIDTH, y: ((e.clientY - r.top) / r.height) * HEIGHT };
}
let aim: { x: number; y: number } | undefined;
canvas.addEventListener("mousemove", (e) => {
  aim = toWorld(e);
});
canvas.addEventListener("mousedown", (e) => {
  aim = toWorld(e);
  mouseDown = true;
});
window.addEventListener("mouseup", () => {
  mouseDown = false;
});

const held = (...codes: string[]) => codes.some((c) => keys.has(c));

// --- simulation ------------------------------------------------------------------------
function shotPos(s: Shot, now: number): { x: number; y: number } {
  const t = (now - s.born) / 1000;
  return { x: s.x + s.vx * t, y: s.y + s.vy * t };
}

function step(now: number, dt: number): void {
  if (self.alive) {
    const turn = Number(held("KeyD", "ArrowRight")) - Number(held("KeyA", "ArrowLeft"));
    const drive = Number(held("KeyW", "ArrowUp")) - Number(held("KeyS", "ArrowDown"));
    self.a += turn * TURN_SPEED * dt;
    const speed = drive * TANK_SPEED * (drive < 0 ? 0.6 : 1) * dt;
    Object.assign(self, move(self.x, self.y, Math.cos(self.a) * speed, Math.sin(self.a) * speed));
    if (aim) self.t = Math.atan2(aim.y - self.y, aim.x - self.x);
    else self.t = lerpAngle(self.t, self.a, Math.min(1, dt * 6));
    if (mouseDown || held("Space")) fire();
  } else if (now >= self.respawnAt) {
    const p = randomSpawn();
    Object.assign(self, p, { t: p.a, hp: 100, alive: true });
    void publish(true);
  }

  const ease = 1 - Math.exp(-dt * 14);
  for (const [id, t] of tanks) {
    if (now - t.seen > STALE_MS) {
      tanks.delete(id);
      continue;
    }
    t.x += (t.target.x - t.x) * ease;
    t.y += (t.target.y - t.y) * ease;
    t.a = lerpAngle(t.a, t.target.a, ease);
    t.t = lerpAngle(t.t, t.target.t, ease);
  }

  const hitR = TANK_RADIUS + SHOT_RADIUS;
  for (const [id, s] of shots) {
    if (now - s.born > s.life) {
      if (!spent.has(id)) blasts.push({ ...shotPos(s, now), color: colorFor(s.owner), born: now, big: false });
      shots.delete(id);
      spent.delete(id);
      continue;
    }
    if (spent.has(id)) continue;
    const p = shotPos(s, now);
    if (s.owner !== me && self.alive && Math.hypot(p.x - self.x, p.y - self.y) < hitR) {
      takeHit(id, s, now);
      continue;
    }
    // Hide shots that visibly hit another tank; that tank's owner decides the damage.
    for (const [tid, t] of tanks) {
      if (tid !== s.owner && t.alive && Math.hypot(p.x - t.x, p.y - t.y) < hitR) {
        spent.add(id);
        blasts.push({ ...p, color: t.color, born: now, big: false });
        break;
      }
    }
  }
  for (let i = blasts.length - 1; i >= 0; i--) if (now - blasts[i].born > 600) blasts.splice(i, 1);
  void publish();
}

// --- drawing ---------------------------------------------------------------------------
function fit(): void {
  const box = canvas.parentElement!.getBoundingClientRect();
  const scale = Math.min(box.width / WIDTH, box.height / HEIGHT);
  const dpr = window.devicePixelRatio || 1;
  canvas.style.width = `${WIDTH * scale}px`;
  canvas.style.height = `${HEIGHT * scale}px`;
  canvas.width = Math.round(WIDTH * scale * dpr);
  canvas.height = Math.round(HEIGHT * scale * dpr);
  ctx.setTransform(scale * dpr, 0, 0, scale * dpr, 0, 0);
}
new ResizeObserver(fit).observe(canvas.parentElement!);

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

function drawTank(
  x: number,
  y: number,
  a: number,
  t: number,
  color: string,
  name: string,
  hp: number,
  mine: boolean,
): void {
  ctx.save();
  ctx.translate(x, y);
  if (mine) {
    ctx.beginPath();
    ctx.arc(0, 0, TANK_RADIUS + 9, 0, Math.PI * 2);
    ctx.strokeStyle = color;
    ctx.globalAlpha = 0.35;
    ctx.lineWidth = 2;
    ctx.stroke();
    ctx.globalAlpha = 1;
  }
  ctx.rotate(a);
  ctx.fillStyle = "rgb(0 0 0 / 0.35)";
  ctx.fillRect(-20, -17, 40, 7);
  ctx.fillRect(-20, 10, 40, 7);
  ctx.fillStyle = color;
  ctx.beginPath();
  ctx.roundRect(-17, -12, 34, 24, 5);
  ctx.fill();
  ctx.rotate(t - a);
  ctx.fillStyle = "rgb(0 0 0 / 0.25)";
  ctx.fillRect(4, -3.5, 24, 7);
  ctx.beginPath();
  ctx.arc(0, 0, 9, 0, Math.PI * 2);
  ctx.fill();
  ctx.restore();

  ctx.font = "600 12px system-ui, sans-serif";
  ctx.textAlign = "center";
  ctx.fillStyle = css("--fg");
  ctx.fillText(mine ? `${name} (you)` : name, x, y - 32);
  ctx.fillStyle = "rgb(127 127 127 / 0.35)";
  ctx.fillRect(x - 20, y - 27, 40, 4);
  ctx.fillStyle = hp > 50 ? css("--ok") : hp > 25 ? css("--warn") : css("--err");
  ctx.fillRect(x - 20, y - 27, (40 * hp) / 100, 4);
}

function draw(now: number): void {
  ctx.fillStyle = css("--card");
  ctx.fillRect(0, 0, WIDTH, HEIGHT);
  ctx.strokeStyle = css("--line");
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let x = 40; x < WIDTH; x += 40) {
    ctx.moveTo(x, 0);
    ctx.lineTo(x, HEIGHT);
  }
  for (let y = 40; y < HEIGHT; y += 40) {
    ctx.moveTo(0, y);
    ctx.lineTo(WIDTH, y);
  }
  ctx.stroke();

  ctx.fillStyle = css("--muted");
  for (const w of WALLS) {
    ctx.beginPath();
    ctx.roundRect(w.x, w.y, w.w, w.h, 4);
    ctx.fill();
  }

  for (const [id, s] of shots) {
    if (spent.has(id)) continue;
    const p = shotPos(s, now);
    ctx.fillStyle = colorFor(s.owner);
    ctx.beginPath();
    ctx.arc(p.x, p.y, SHOT_RADIUS, 0, Math.PI * 2);
    ctx.fill();
  }

  for (const t of tanks.values()) if (t.alive) drawTank(t.x, t.y, t.a, t.t, t.color, t.name, t.hp, false);
  if (self.alive) drawTank(self.x, self.y, self.a, self.t, myColor, self.name, self.hp, true);

  for (const b of blasts) {
    const k = (now - b.born) / 600;
    ctx.globalAlpha = 1 - k;
    ctx.fillStyle = b.color;
    ctx.beginPath();
    ctx.arc(b.x, b.y, (b.big ? 46 : 12) * (0.4 + k), 0, Math.PI * 2);
    ctx.fill();
    ctx.globalAlpha = 1;
  }

  if (!self.alive) {
    ctx.fillStyle = "rgb(0 0 0 / 0.35)";
    ctx.fillRect(0, 0, WIDTH, HEIGHT);
    ctx.fillStyle = "#fff";
    ctx.font = "700 34px system-ui, sans-serif";
    ctx.textAlign = "center";
    ctx.fillText(`Respawning in ${Math.ceil((self.respawnAt - now) / 1000)}…`, WIDTH / 2, HEIGHT / 2);
  }
}

let last = performance.now();
function frame(now: number): void {
  const dt = Math.min(0.05, (now - last) / 1000);
  last = now;
  step(now, dt);
  draw(now);
  requestAnimationFrame(frame);
}

// --- HUD -------------------------------------------------------------------------------
function renderHud(): void {
  const score = new Map<string, { name: string; kills: number; deaths: number }>();
  const row = (id: string, name: string) => {
    const r = score.get(id) ?? { name, kills: 0, deaths: 0 };
    score.set(id, r);
    return r;
  };
  row(me, self.name).name = self.name;
  for (const [id, t] of tanks) row(id, t.name).name = t.name;
  for (const k of kills) {
    row(k.killer, k.killerName).kills++;
    row(k.victim, k.victimName).deaths++;
  }
  const ranked = [...score].sort((a, b) => b[1].kills - a[1].kills || a[1].deaths - b[1].deaths).slice(0, 12);
  $("#scores").replaceChildren(
    ...ranked.map(([id, r]) =>
      h(
        "tr",
        { class: id === me ? "me" : "" },
        h("td", {}, h("span", { class: "dot", style: `background:${colorFor(id)}` }), r.name),
        h("td", { text: String(r.kills) }),
        h("td", { text: String(r.deaths) }),
      ),
    ),
  );
  $("#feed").replaceChildren(
    ...kills
      .slice(0, 6)
      .map((k) =>
        h(
          "li",
          {},
          h("strong", { style: `color:${colorFor(k.killer)}`, text: k.killerName }),
          " destroyed ",
          h("strong", { style: `color:${colorFor(k.victim)}`, text: k.victimName }),
        ),
      ),
  );
  $("#players").textContent = String(tanks.size + 1);
}

const nameInput = $<HTMLInputElement>("#name");
nameInput.value = self.name;
nameInput.addEventListener("input", () => {
  const name = nameInput.value.trim().slice(0, 24);
  if (!name) return;
  self.name = name;
  saveName(name);
});
nameInput.addEventListener("keydown", (e) => e.key === "Enter" && nameInput.blur());

$<HTMLFormElement>("#room-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const next = cleanRoom($<HTMLInputElement>("#room").value);
  if (next) location.hash = `room=${next}`;
  $<HTMLInputElement>("#room").blur();
});

const roomFromHash = () => cleanRoom(new URLSearchParams(location.hash.slice(1)).get("room") ?? "") || "arena";
window.addEventListener("hashchange", () => enterRoom(roomFromHash()));
window.addEventListener("pagehide", () => void setDoc(myDoc(), { alive: false, at: 0 }, { merge: true }));
setInterval(renderHud, 500);

enterRoom(roomFromHash());
fit();
requestAnimationFrame(frame);
