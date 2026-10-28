// Arena geometry and the deterministic parts of the simulation. Every peer
// runs the same code, so a shot's path is computed locally from its launch
// parameters instead of being streamed.

export const WIDTH = 1200;
export const HEIGHT = 800;
export const TANK_RADIUS = 18;
export const TANK_SPEED = 150;
export const TURN_SPEED = 2.8;
export const SHOT_SPEED = 460;
export const SHOT_LIFE_MS = 1600;
export const SHOT_RADIUS = 4;
export const FIRE_COOLDOWN_MS = 450;
export const HIT_DAMAGE = 34;
export const RESPAWN_MS = 2000;

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export const WALLS: Rect[] = [
  // Center cross.
  { x: 560, y: 300, w: 80, h: 200 },
  { x: 480, y: 370, w: 240, h: 60 },
  // Corner bunkers.
  { x: 160, y: 140, w: 160, h: 28 },
  { x: 160, y: 140, w: 28, h: 120 },
  { x: 880, y: 140, w: 160, h: 28 },
  { x: 1012, y: 140, w: 28, h: 120 },
  { x: 160, y: 632, w: 160, h: 28 },
  { x: 160, y: 540, w: 28, h: 120 },
  { x: 880, y: 632, w: 160, h: 28 },
  { x: 1012, y: 540, w: 28, h: 120 },
  // Side pillars.
  { x: 300, y: 360, w: 60, h: 80 },
  { x: 840, y: 360, w: 60, h: 80 },
  { x: 570, y: 90, w: 60, h: 60 },
  { x: 570, y: 650, w: 60, h: 60 },
];

/** True if a circle overlaps a wall or leaves the arena. */
export function blocked(x: number, y: number, r: number): boolean {
  if (x < r || y < r || x > WIDTH - r || y > HEIGHT - r) return true;
  for (const w of WALLS) {
    const cx = Math.max(w.x, Math.min(x, w.x + w.w));
    const cy = Math.max(w.y, Math.min(y, w.y + w.h));
    if ((x - cx) ** 2 + (y - cy) ** 2 < r * r) return true;
  }
  return false;
}

/** How long a shot flies before hitting a wall or expiring, in ms. */
export function shotLifetime(x: number, y: number, vx: number, vy: number): number {
  const step = 4;
  for (let t = 0; t <= SHOT_LIFE_MS; t += step) {
    if (blocked(x + (vx * t) / 1000, y + (vy * t) / 1000, SHOT_RADIUS)) return t;
  }
  return SHOT_LIFE_MS;
}

export function randomSpawn(): { x: number; y: number; a: number } {
  for (;;) {
    const x = 60 + Math.random() * (WIDTH - 120);
    const y = 60 + Math.random() * (HEIGHT - 120);
    if (!blocked(x, y, TANK_RADIUS * 2)) return { x, y, a: Math.random() * Math.PI * 2 };
  }
}

/** Moves a tank, sliding along walls instead of stopping dead. */
export function move(x: number, y: number, dx: number, dy: number): { x: number; y: number } {
  const nx = blocked(x + dx, y, TANK_RADIUS) ? x : x + dx;
  const ny = blocked(nx, y + dy, TANK_RADIUS) ? y : y + dy;
  return { x: nx, y: ny };
}

/** Shortest-path interpolation between two angles. */
export function lerpAngle(a: number, b: number, t: number): number {
  const d = Math.atan2(Math.sin(b - a), Math.cos(b - a));
  return a + d * t;
}
