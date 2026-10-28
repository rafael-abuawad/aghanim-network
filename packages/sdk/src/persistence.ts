/** Durable storage for the engine's key-value entries. */
export interface Persistence {
  /** All stored entries, as parallel key and value arrays. */
  load(): Promise<[Uint8Array[], Uint8Array[]]>;
  /** Applies a batch of puts (value) and deletes (null) atomically. */
  apply(entries: [Uint8Array, Uint8Array | null][]): Promise<void>;
  close(): void;
}

export class MemoryPersistence implements Persistence {
  async load(): Promise<[Uint8Array[], Uint8Array[]]> {
    return [[], []];
  }
  async apply(): Promise<void> {}
  close(): void {}
}

const STORE = "kv";

function req<T>(r: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

/** IndexedDB persistence: one object store with binary keys. */
export class IdbPersistence implements Persistence {
  private constructor(private db: IDBDatabase) {}

  static async open(name: string): Promise<IdbPersistence> {
    const r = indexedDB.open(name, 1);
    r.onupgradeneeded = () => r.result.createObjectStore(STORE);
    return new IdbPersistence(await req(r));
  }

  async load(): Promise<[Uint8Array[], Uint8Array[]]> {
    const tx = this.db.transaction(STORE, "readonly");
    const store = tx.objectStore(STORE);
    // Both lists come back in key order from the same snapshot.
    const [keys, values] = await Promise.all([req(store.getAllKeys()), req(store.getAll())]);
    return [keys.map((k) => new Uint8Array(k as ArrayBuffer)), values.map((v) => new Uint8Array(v as ArrayBuffer))];
  }

  apply(entries: [Uint8Array, Uint8Array | null][]): Promise<void> {
    if (entries.length === 0) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const tx = this.db.transaction(STORE, "readwrite");
      const store = tx.objectStore(STORE);
      for (const [k, v] of entries) {
        const key = k.slice().buffer;
        if (v === null) store.delete(key);
        else store.put(v.slice().buffer, key);
      }
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
      tx.onabort = () => reject(tx.error);
    });
  }

  close(): void {
    this.db.close();
  }
}
