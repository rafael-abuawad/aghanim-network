import "fake-indexeddb/auto";
import { expect, it } from "vitest";
import { doc, getDoc, setDoc } from "../src/index.ts";
import { open } from "./helpers.ts";

it("persists data, device key and login in IndexedDB", async () => {
  const appId = `persist-${Math.random()}`;
  const db1 = await open({ appId, persistence: "indexeddb" });
  const user = await db1.auth.login("alice", "correct horse battery staple");
  await setDoc(doc(db1, "public/p/c/d"), { v: 42 });
  const device = db1.deviceId;
  await db1.close();

  const db2 = await open({ appId, persistence: "indexeddb" });
  expect(db2.deviceId).toBe(device);
  expect(db2.auth.userId).toBe(user);
  expect((await getDoc(doc(db2, "public/p/c/d"))).get("v")).toBe(42);
  await db2.close();

  // Same alias and passphrase on a fresh device gives the same identity.
  const fresh = await open({ appId: `${appId}-other` });
  expect(await fresh.auth.login("alice", "correct horse battery staple")).toBe(user);
  expect(await fresh.auth.login("alice", "wrong")).not.toBe(user);
  await fresh.close();
});
