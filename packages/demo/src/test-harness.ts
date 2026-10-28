// Exposes a small driver API for the Playwright end-to-end tests.
import {
  collection,
  type Db,
  doc,
  getDoc,
  type OpenOptions,
  onSnapshot,
  openDb,
  orderBy,
  query,
  rawDoc,
  setDoc,
} from "@aghanim/sdk";

let db: Db;
const events: { path: string; data: unknown; at: number }[] = [];

const harness = {
  async open(opts: Partial<OpenOptions> & { appId: string; relay: string }) {
    db = await openDb({ ...opts, relays: [opts.relay] });
    return { deviceId: db.deviceId, userId: db.auth.userId };
  },
  async write(path: string, data: Record<string, unknown>, merge = true) {
    await setDoc(doc(db, path), data, { merge });
  },
  async tryWrite(path: string, data: Record<string, unknown>) {
    try {
      await setDoc(doc(db, path), data, { merge: true });
      return "ok";
    } catch (e) {
      return String(e instanceof Error ? e.message : e);
    }
  },
  async get(path: string) {
    return (await getDoc(doc(db, path))).data() ?? null;
  },
  /** Records every snapshot of a collection into `events`. */
  watch(collPath: string) {
    onSnapshot(query(collection(db, collPath), orderBy("at")), (s) => {
      for (const c of s.docChanges()) events.push({ path: c.doc.ref.path, data: c.doc.data(), at: Date.now() });
    });
  },
  events: () => events,
  status: () => db.network.status(),
  offline: () => db.network.goOffline(),
  online: () => db.network.goOnline(),
  waitForSync: (path: string, ms = 5000) => db.network.waitForSync(path, ms),
  createSpace: async (name: string) => (await db.spaces.create(name)).id,
  openSpace: async (id: string) => (await db.spaces.open(id)).id,
  invite: (space: string, pk: string, role: "writer" | "reader" = "writer") => db.spaces.invite(space, pk, role),
  remove: (space: string, pk: string) => db.spaces.remove(space, pk),
  members: (space: string) => db.spaces.members(space),
  raw: (path: string) =>
    rawDoc(db, path).then((r) => JSON.stringify(r, (_, v) => (typeof v === "bigint" ? Number(v) : v))),
  userId: () => db.auth.userId,
};

(window as unknown as { harness: typeof harness }).harness = harness;
document.title = "harness ready";
