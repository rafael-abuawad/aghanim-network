import { readFileSync } from "node:fs";
import { join } from "node:path";
import { type Browser, expect, type Page, test } from "@playwright/test";
import { DATA_DIR, RELAY } from "./playwright.config.ts";

// The harness page (packages/demo/src/test-harness.ts) exposes `window.harness`.
type Harness = Record<string, (...args: never[]) => unknown>;
declare global {
  interface Window {
    harness: Harness & Record<string, any>;
  }
}

interface Peer {
  page: Page;
  deviceId: string;
  userId: string;
}

const uid = () => Math.random().toString(36).slice(2, 10);

async function peer(
  browser: Browser,
  appId: string,
  opts: Record<string, unknown> = {},
  init?: () => void,
): Promise<Peer> {
  const ctx = await browser.newContext();
  if (init) await ctx.addInitScript(init);
  const page = await ctx.newPage();
  page.on("pageerror", (e) => console.log(`[${appId} pageerror]`, e.message));
  await page.goto("/test.html");
  await page.waitForFunction(() => document.title === "harness ready");
  const ids = await page.evaluate((o) => window.harness.open(o), { appId, relay: RELAY, ...opts });
  return { page, ...(ids as { deviceId: string; userId: string }) };
}

const rtcLinks = (p: Peer) =>
  p.page.evaluate(() => window.harness.status().links.filter((l: any) => l.kind === "webrtc" && l.peer).length);

test("a write reaches the other browser over direct WebRTC within 1 s", async ({ browser }) => {
  const app = `rtc-${uid()}`;
  // Relays are used only for signaling here, so data can only flow over WebRTC.
  const a = await peer(browser, app, { syncWithRelays: false });
  const b = await peer(browser, app, { syncWithRelays: false });
  await expect.poll(() => rtcLinks(a), { timeout: 20_000 }).toBe(1);
  await expect.poll(() => rtcLinks(b), { timeout: 20_000 }).toBe(1);

  await b.page.evaluate(() => window.harness.watch("public/rtc/items"));
  await a.page.evaluate(() => window.harness.waitForSync("public/rtc", 3000));
  const started = Date.now();
  await a.page.evaluate(() => window.harness.write("public/rtc/items/i1", { v: "hello", at: 1 }));
  await expect
    .poll(() => b.page.evaluate(() => window.harness.events().filter((e: any) => e.data?.v === "hello").length), {
      timeout: 1000,
      intervals: [20],
    })
    .toBe(1);
  expect(Date.now() - started).toBeLessThan(1000);
  const status = await b.page.evaluate(() => window.harness.status());
  expect(status.links.find((l: any) => l.kind === "webrtc")?.peer).toBe(a.deviceId);
});

test("offline edits on both sides converge after reconnecting", async ({ browser }) => {
  const app = `offline-${uid()}`;
  const a = await peer(browser, app);
  const b = await peer(browser, app);
  const path = "public/notes/docs/d1";
  await a.page.evaluate((p) => window.harness.write(p, { x: 1 }), path);
  await expect.poll(() => b.page.evaluate((p) => window.harness.get(p), path), { timeout: 10_000 }).toEqual({ x: 1 });

  await a.page.evaluate(() => window.harness.offline());
  await b.page.evaluate(() => window.harness.offline());
  await a.page.evaluate((p) => window.harness.write(p, { x: 2, fromA: true }), path);
  await b.page.evaluate((p) => window.harness.write(p, { y: 3, fromB: true }), path);
  // Still apart.
  expect(await b.page.evaluate((p) => window.harness.get(p), path)).toEqual({ x: 1, y: 3, fromB: true });

  await a.page.evaluate(() => window.harness.online());
  await b.page.evaluate(() => window.harness.online());
  const want = { x: 2, y: 3, fromA: true, fromB: true };
  await expect.poll(() => a.page.evaluate((p) => window.harness.get(p), path), { timeout: 15_000 }).toEqual(want);
  await expect.poll(() => b.page.evaluate((p) => window.harness.get(p), path), { timeout: 15_000 }).toEqual(want);
});

test("nobody else can write to a user's space", async ({ browser }) => {
  const app = `users-${uid()}`;
  const a = await peer(browser, app);
  const b = await peer(browser, app);
  const path = `users/${a.userId}/profile/main`;
  await a.page.evaluate((p) => window.harness.write(p, { name: "Alice" }), path);
  const err = await b.page.evaluate((p) => window.harness.tryWrite(p, { name: "Mallory" }), path);
  expect(err).toMatch(/permission denied/);
  await expect
    .poll(() => b.page.evaluate((p) => window.harness.get(p), path), { timeout: 10_000 })
    .toEqual({ name: "Alice" });
  expect(await a.page.evaluate((p) => window.harness.get(p), path)).toEqual({ name: "Alice" });
});

test("the relay stores only ciphertext for space content", async ({ browser }) => {
  const app = `vault-${uid()}`;
  const a = await peer(browser, app);
  const space = (await a.page.evaluate(() => window.harness.createSpace("vault"))) as string;
  const secret = `TOP-SECRET-${uid()}`;
  const marker = `PUBLIC-MARK-${uid()}`;
  await a.page.evaluate(([s, t]) => window.harness.write(`spaces/${s}/notes/n1`, { text: t }), [space, secret]);
  await a.page.evaluate((t) => window.harness.write("public/marks/m/x", { text: t }), marker);

  const db = () => readFileSync(join(DATA_DIR, "relay.redb"));
  await expect.poll(() => db().includes(marker), { timeout: 10_000 }).toBe(true);

  // A non-member with WebRTC off can only get data from the relay. It
  // receives the note (as ciphertext), which proves the relay stored it…
  const c = await peer(browser, app, { webrtc: false });
  await c.page.evaluate((s) => window.harness.openSpace(s), space);
  const path = `spaces/${space}/notes/n1`;
  await expect.poll(() => c.page.evaluate((p) => window.harness.raw(p), path), { timeout: 10_000 }).toContain('"text"');
  expect(await c.page.evaluate((p) => window.harness.get(p), path)).toBeNull();
  // …yet the plaintext is nowhere in the relay's database file.
  expect(db().includes(secret)).toBe(false);
  expect(db().includes(space)).toBe(true); // paths are visible metadata
});

test("removing a member rotates the key", async ({ browser }) => {
  const app = `rotate-${uid()}`;
  const a = await peer(browser, app);
  const b = await peer(browser, app);
  const space = (await a.page.evaluate(() => window.harness.createSpace("team"))) as string;
  await a.page.evaluate(([s, pk]) => window.harness.invite(s, pk), [space, b.userId]);
  await b.page.evaluate((s) => window.harness.openSpace(s), space);
  const path = `spaces/${space}/todos/t1`;
  await a.page.evaluate((p) => window.harness.write(p, { title: "first secret" }), path);
  await expect
    .poll(() => b.page.evaluate((p) => window.harness.get(p), path), { timeout: 10_000 })
    .toEqual({
      title: "first secret",
    });

  await a.page.evaluate(([s, pk]) => window.harness.remove(s, pk), [space, b.userId]);
  await a.page.evaluate((p) => window.harness.write(p, { plan: "second secret" }), path);
  // Bob still receives the new field, but only as ciphertext he can't open.
  await expect.poll(() => b.page.evaluate((p) => window.harness.raw(p), path), { timeout: 10_000 }).toContain('"plan"');
  expect(await b.page.evaluate((p) => window.harness.get(p), path)).toEqual({ title: "first secret" });
  expect(await b.page.evaluate((p) => window.harness.tryWrite(p, { x: 1 }), path)).toMatch(/permission denied/);
  expect(await a.page.evaluate((p) => window.harness.get(p), path)).toEqual({
    title: "first secret",
    plan: "second secret",
  });
});

test("when WebRTC can't connect, sync falls back to the relay", async ({ browser }) => {
  const app = `fallback-${uid()}`;
  const a = await peer(browser, app);
  // Simulate blocked UDP: B may only use TURN relays, and has none, so ICE
  // gathers no usable candidates and the DataChannel never opens.
  const b = await peer(browser, app, {}, () => {
    const Native = window.RTCPeerConnection;
    window.RTCPeerConnection = class extends Native {
      constructor(cfg?: RTCConfiguration) {
        super({ ...cfg, iceServers: [], iceTransportPolicy: "relay" });
      }
    } as typeof RTCPeerConnection;
  });
  await b.page.evaluate(() => window.harness.watch("public/fb/items"));
  await a.page.evaluate(() => window.harness.write("public/fb/items/i1", { v: "via relay", at: 1 }));
  await expect
    .poll(() => b.page.evaluate(() => window.harness.events().some((e: any) => e.data?.v === "via relay")), {
      timeout: 10_000,
    })
    .toBe(true);
  expect(await rtcLinks(b)).toBe(0);
  const relayLinks = await b.page.evaluate(
    () => window.harness.status().links.filter((l: any) => l.kind === "relay").length,
  );
  expect(relayLinks).toBe(1);
});

test("demo app: chat and an encrypted todo list between two browsers", async ({ browser }) => {
  const alerts: string[] = [];
  const open = async () => {
    const page = await (await browser.newContext()).newPage();
    page.on("dialog", (d) => {
      alerts.push(d.message());
      void d.dismiss();
    });
    await page.goto(`/?relay=${encodeURIComponent(RELAY)}`);
    await expect(page.locator("#me")).toHaveText(/^[0-9a-f]{64}$/);
    return page;
  };
  const alice = await open();
  const bob = await open();

  const text = `hello ${uid()}`;
  await alice.locator("#say input").fill(text);
  await alice.locator("#say button").click();
  await expect(bob.locator("#messages")).toContainText(text, { timeout: 10_000 });

  await alice.locator("#create-space").click();
  await expect(alice.locator("#in-space")).toBeVisible();
  const spaceUrl = alice.url();
  const bobId = (await bob.locator("#me").textContent())!;
  await alice.locator("#space details summary").click();
  await alice.locator("#invite input[name=pk]").fill(bobId);
  await alice.locator("#invite button").click();
  await expect(alice.locator("#members")).toContainText(`writer ${bobId.slice(0, 16)}`);
  await alice.locator("#add-todo input").fill("buy milk");
  await alice.locator("#add-todo button").click();

  // Opening the invite link (a hash change on the already-open page).
  await bob.goto(spaceUrl);
  await expect(bob.locator("#todos")).toContainText("buy milk", { timeout: 10_000 });
  await bob.locator("#todos input[type=checkbox]").first().check();
  await expect(alice.locator("#todos li.done")).toContainText("buy milk", { timeout: 10_000 });
  // Alice and Bob found each other through the relay and connected directly.
  await alice.bringToFront();
  await expect(alice.locator("#net")).toContainText("relay: open · webrtc peers: 1", { timeout: 10_000 });
  await alice.screenshot({ path: "test-results/demo-alice.png", fullPage: true });
  expect(alerts).toEqual([]);
});
