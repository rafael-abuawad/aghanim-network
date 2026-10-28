import { afterEach, describe, expect, it } from "vitest";
import {
  addDoc,
  collection,
  type Db,
  deleteDoc,
  deleteField,
  doc,
  getDoc,
  getDocs,
  limit,
  onSnapshot,
  orderBy,
  type QuerySnapshot,
  query,
  setDoc,
  updateDoc,
  where,
  writeBatch,
} from "../src/index.ts";
import { open, pair, until } from "./helpers.ts";

const dbs: Db[] = [];
async function make(...args: Parameters<typeof open>) {
  const db = await open(...args);
  dbs.push(db);
  return db;
}
afterEach(async () => {
  await Promise.all(dbs.splice(0).map((d) => d.close()));
});

describe("documents and queries", () => {
  it("creates an anonymous user and does CRUD", async () => {
    const db = await make();
    expect(db.auth.userId).toMatch(/^[0-9a-f]{64}$/);
    const ref = doc(db, "public/app/todos/t1");
    await setDoc(ref, { title: "milk", done: false, n: 1.5, tags: ["a"], meta: { x: 1 }, bin: new Uint8Array([1, 2]) });
    let snap = await getDoc(ref);
    expect(snap.exists()).toBe(true);
    expect(snap.data()).toEqual({
      title: "milk",
      done: false,
      n: 1.5,
      tags: ["a"],
      meta: { x: 1 },
      bin: new Uint8Array([1, 2]),
    });

    await updateDoc(ref, { done: true, title: deleteField() });
    snap = await getDoc(ref);
    expect(snap.get("done")).toBe(true);
    expect(snap.get("title")).toBeUndefined();

    await setDoc(ref, { only: 1 });
    expect((await getDoc(ref)).data()).toEqual({ only: 1 });

    await deleteDoc(ref);
    expect((await getDoc(ref)).exists()).toBe(false);
    await expect(updateDoc(ref, { a: 1 })).rejects.toThrow(/not found/);
  });

  it("queries with where, orderBy and limit", async () => {
    const db = await make();
    const todos = collection(db, "public/app/todos");
    const batch = writeBatch(db);
    for (const [i, n] of [5, 3, 9, 1].entries()) batch.set(doc(todos, `t${i}`), { n, done: n > 4 });
    await batch.commit();
    await addDoc(todos, { n: 7, done: true });

    const ids = (s: QuerySnapshot) => s.docs.map((d) => d.get("n"));
    expect(ids(await getDocs(query(todos, orderBy("n"))))).toEqual([1, 3, 5, 7, 9]);
    expect(ids(await getDocs(query(todos, orderBy("n", "desc"), limit(2))))).toEqual([9, 7]);
    expect(ids(await getDocs(query(todos, where("done", "==", true), orderBy("n"))))).toEqual([5, 7, 9]);
    expect(ids(await getDocs(query(todos, where("n", "in", [1, 9]), orderBy("n"))))).toEqual([1, 9]);
    expect((await getDocs(query(todos, where("n", ">=", 5)))).size).toBe(3);
    await expect(getDocs(collection(db, "nope/x/y"))).rejects.toThrow();
  });

  it("rejects writes to someone else's user space", async () => {
    const a = await make();
    const b = await make();
    await expect(setDoc(doc(b, `users/${a.auth.userId}/profile/main`), { name: "x" })).rejects.toThrow(
      /permission denied/,
    );
    await setDoc(doc(b, `users/${b.auth.userId}/profile/main`), { name: "me" });
  });
});

describe("listeners and sync", () => {
  it("delivers local and remote changes to onSnapshot", async () => {
    const a = await make();
    const b = await make();
    const coll = collection(b, "public/chat/messages");
    const seen: QuerySnapshot[] = [];
    const unsub = onSnapshot(query(coll, orderBy("at")), (s) => seen.push(s));
    await until(() => seen.length === 1);
    expect(seen[0]!.empty).toBe(true);
    expect(seen[0]!.metadata.fromCache).toBe(true);

    await setDoc(doc(a, "public/chat/messages/m1"), { text: "hi", at: 1 });
    pair(a, b);
    await until(() => seen.some((s) => s.size === 1));
    const added = seen.find((s) => s.size === 1)!;
    expect(added.docChanges().map((c) => [c.type, c.doc.id])).toEqual([["added", "m1"]]);
    expect(await b.network.waitForSync("public/chat", 2000)).toBe(true);

    await setDoc(doc(a, "public/chat/messages/m1"), { text: "edited", at: 1 });
    await until(() => seen.some((s) => s.docChanges()[0]?.type === "modified"));
    await deleteDoc(doc(b, "public/chat/messages/m1"));
    await until(() => seen.some((s) => s.docChanges()[0]?.type === "removed"));
    const n = seen.length;
    unsub();
    await setDoc(doc(a, "public/chat/messages/m2"), { text: "late", at: 2 });
    await new Promise((r) => setTimeout(r, 50));
    expect(seen.length).toBe(n);
    // …but the data still syncs.
    await until(async () => (await getDoc(doc(b, "public/chat/messages/m2"))).exists());
  });

  it("converges after both sides edit offline", async () => {
    const a = await make();
    const b = await make();
    const ref = (db: Db) => doc(db, "public/app/docs/d");
    await setDoc(ref(a), { x: 1 });
    const link = pair(a, b);
    await until(async () => (await getDoc(ref(b))).exists());
    link.cut();
    await updateDoc(ref(a), { x: 2, fromA: true });
    await updateDoc(ref(b), { y: 3, fromB: true });
    pair(a, b);
    await until(async () => (await getDoc(ref(a))).get("fromB") && (await getDoc(ref(b))).get("fromA"));
    expect((await getDoc(ref(a))).data()).toEqual((await getDoc(ref(b))).data());
    expect((await getDoc(ref(a))).data()).toEqual({ x: 2, y: 3, fromA: true, fromB: true });
  });
});

describe("encrypted spaces", () => {
  it("shares with members, hides from others, and rotates on removal", async () => {
    const owner = await make();
    const bob = await make();
    const eve = await make();
    pair(owner, bob);
    pair(owner, eve);
    const space = await owner.spaces.create("team");
    await owner.spaces.invite(space, bob.auth.userId!, "writer");
    await bob.spaces.open(space.id);
    await eve.spaces.open(space.id);

    const ref = (db: Db) => doc(db, `spaces/${space.id}/todos/t1`);
    await setDoc(ref(owner), { title: "secret plan" });
    await until(async () => (await getDoc(ref(bob))).get("title") === "secret plan");
    // Eve syncs the space too, but can't decrypt anything.
    await until(() => eve.network.waitForSync(`spaces/${space.id}`, 50));
    expect((await getDoc(ref(eve))).exists()).toBe(false);
    await expect(setDoc(ref(eve), { title: "x" })).rejects.toThrow(/permission denied/);

    await setDoc(ref(bob), { by: "bob" }, { merge: true });
    await until(async () => (await getDoc(ref(owner))).get("by") === "bob");
    expect((await owner.spaces.members(space)).map((m) => m.role).sort()).toEqual(["owner", "writer"]);

    await owner.spaces.remove(space, bob.auth.userId!);
    await setDoc(ref(owner), { plan: "after removal" }, { merge: true });
    await until(async () => (await getDoc(ref(eve))) && (await owner.spaces.members(space)).length === 2);
    await new Promise((r) => setTimeout(r, 100));
    const bobView = (await getDoc(ref(bob))).data();
    expect(bobView?.title).toBe("secret plan");
    expect(bobView?.plan).toBeUndefined();
    expect((await getDoc(ref(owner))).get("plan")).toBe("after removal");
  });
});
