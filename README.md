# aghanim-network

A decentralized, end-to-end encrypted, Firestore-style database. Browsers sync
directly with each other over WebRTC; optional relays handle introductions,
keep data around when every browser is closed, and carry traffic when a direct
connection isn't possible. The engine is Rust compiled to WebAssembly, and the
same code runs natively in the relay.

```ts
import { collection, doc, onSnapshot, openDb, orderBy, query, setDoc, where } from "@aghanim/sdk";

const db = await openDb({ appId: "todo", relays: ["wss://relay.example.com"] });

const space = await db.spaces.create("team");          // end-to-end encrypted
await db.spaces.invite(space, bobPublicKey, "writer");

const todos = collection(db, `spaces/${space.id}/todos`);
await setDoc(doc(todos, "t1"), { title: "milk", done: false, at: Date.now() });

onSnapshot(query(todos, where("done", "==", false), orderBy("at")), (snap) => {
  for (const change of snap.docChanges()) console.log(change.type, change.doc.data());
});
```

## Why not Gun?

| | Gun | aghanim-network |
|---|---|---|
| Conflicts | HAM: state-machine timestamps, hard to reason about | Per-field last-writer-wins on a hybrid logical clock; tested to converge |
| Data model | Graph of nodes | Collections and documents, like Firestore |
| Queries | None built in | `where` / `orderBy` / `limit`, with automatic single-field indexes |
| Crypto | SEA, in JS | Ed25519, X25519, XChaCha20-Poly1305, Argon2id, BLAKE3, in WASM |
| Write rules | Per-app conventions | Enforced by every peer: signed changes and namespace rules |
| Transport | WebSocket mesh through relays | WebRTC between browsers; relays for signaling, storage and fallback |
| Sync | Gossip | Cursor-based catch-up: reconnecting transfers only what you missed |

## Quick start

You need Rust (with the `wasm32-unknown-unknown` target), `wasm-bindgen-cli`
matching the `wasm-bindgen` version in `crates/wasm/Cargo.toml` (0.2.129),
Node 22+ and pnpm. `wasm-opt` (binaryen) and `just` are optional.

```sh
pnpm install
node scripts/build-wasm.mjs        # Rust engine → packages/sdk/wasm
just relay                         # or: cargo run -p aghanim-relay --release
pnpm --filter @aghanim/demo dev    # http://localhost:5173
```

Open the demo in two browsers (or a normal and a private window). The public
chat syncs right away. Create a space in one, paste the other's user id into
**Members → Invite**, then open the page's URL (it carries the space id) in the
other browser.

### Playground

```sh
pnpm --filter @aghanim/playground dev    # http://localhost:5174
```

The playground has four pages:

- **Scripts** (`/`) runs SDK snippets with no relay needed. Two peers, `alice`
  and `bob`, run in the page with their own engines and in-memory stores,
  joined by a link you can cut and restore. Pick an example (live sync,
  queries, offline edits, write permissions, encrypted spaces, identity), edit
  it, and run it with Ctrl+Enter.
- **Chat** (`/chat/`): public rooms with display names, an online list and
  typing indicators. History is stored on the device and readable offline.
- **Discord** (`/discord/`): a public lobby plus servers you create, each an
  end-to-end encrypted space with channels. Share an invite link; opening it
  files a join request that the owner approves. Removing a member rotates the
  key.
- **Tanks** (`/tanks/`): a real-time multiplayer arena. Each tank publishes
  its own state about 12 times a second, shots are simulated locally from their
  launch, and the tank that gets hit decides the damage, so no peer writes
  anyone else's state.

The three apps need a relay (`just relay`, or `?relay=wss://…` in the URL) to
find peers in other browsers; tabs of one browser also sync with each other
directly. Chat and Discord keep one identity per browser, so use two browsers
(or a private window) to talk to yourself. Tanks gives every tab its own tank.
Tanks rooms keep every position update in the relay's change log, so use fresh
room names (`#room=…`) for long sessions.

## Layout

```
crates/
  core/     sans-io data model: CBOR codec, values, HLC, documents and LWW merge,
            key encoding, queries, signed change format
  crypto/   identities, passphrase login, change signatures, field encryption,
            space key wrapping
  store/    ordered key-value trait, in-memory store with write journal,
            documents, indexes, per-scope change log, cursors, query planner
  sync/     the sync node: write path, authorization rules, spaces, sessions,
            handshake, catch-up and live push
  wasm/     wasm-bindgen bindings for the engine
  relay/    aghanim-relay: axum WebSocket server, redb storage, signaling
packages/
  sdk/      @aghanim/sdk: Firestore-style API, Web Worker host, IndexedDB,
            WebRTC / WebSocket / BroadcastChannel transports
  demo/     @aghanim/demo: public chat and an encrypted todo list (Vite)
playground/ @aghanim/playground: runnable SDK scripts, plus chat, Discord-style
            and tank game demo apps
tests/e2e/  Playwright tests with real browsers, a real relay and the demo
```

## How it works

### Data model

Paths alternate collections and documents: `collection/doc[/collection/doc…]`.
The first two segments are the **scope**, which is the unit of sync and of
authorization. The root collection picks the rules:

| Namespace | Who can write | Values |
|---|---|---|
| `public/<name>/…` | Anyone (writes are still signed) | Plaintext |
| `users/<pubkey>/…` | Only that key | Plaintext |
| `spaces/<id>/…` | The owner and members with the `writer` role | Encrypted (enforced) |

Each document field stores a cell `{value, hlc, author}`. Concurrent writes
resolve per field: the highest `(hlc, author)` wins. Deleting a document records
a tombstone that hides every older field, so a late, stale write can't bring
deleted data back. This merge is commutative, associative and idempotent. A
property test and a network simulator check that peers converge whatever order
changes arrive in, including duplicates.

The hybrid logical clock packs 48 bits of milliseconds and a 16-bit counter.
Peers reject changes stamped more than 5 minutes ahead of their own clock, so
nobody can win every conflict by setting their clock to next year.

### Security

- **Identity.** One Ed25519 key per user. Its public key (hex) is the user id.
  Users can be anonymous (a random key, created automatically) or log in with
  `alias + passphrase`. The key is derived with Argon2id (19 MiB, 2 passes), so
  the same pair gives the same identity on any device, like Gun SEA's login.
- **Signed changes.** A batch of writes to one scope is one `Change`, signed once
  over its BLAKE3 hash. Every peer, relays included, checks shape, signature,
  clock drift and namespace rules before storing or forwarding a change.
- **Spaces.** `spaces/<id>` is the space's ACL document: `owner`, `salt`,
  `epoch`, member grants `m.<pubkey> = {role, from, until?}`, and the space key
  wrapped to each member `k.<epoch>.<pubkey>`. The space id is derived from the
  owner's key and the salt, so anyone can verify who owns a space. Content
  values are encrypted with XChaCha20-Poly1305, with the document path, field
  name and key epoch as associated data. Space keys are wrapped with an
  ephemeral X25519 sealed box to each member's key (the X25519 form of their
  Ed25519 key, as libsodium and `age` do). Removing a member rotates the key.
- **Peer handshake.** Each connection starts with a signed challenge, so a peer
  is authenticated as its device key. A relay only lets a connection join
  signaling under the key it proved it holds.

**What a relay (or any peer) can see:** paths (including space ids and document
ids), field names, timestamps, author public keys, and who is online. It can't
see the values in spaces. Queries on spaces run on the client after decryption
and can't use indexes.

### Sync protocol

Every link (a WebRTC DataChannel, a relay WebSocket, a BroadcastChannel between
tabs) is a reliable, ordered byte pipe carrying CBOR frames:

1. `Hello` / `Proof`: each side signs the other's nonce.
2. `Sub {scope, cursor}`: "send me everything in this scope after `cursor`".
   `cursor` is a sequence number in the responder's change log, saved per peer,
   so a reconnect only transfers the gap.
3. The responder streams `Changes`, sends `Synced`, then pushes new changes
   live. Relays subscribe back, so they store everything their clients write.
4. Changes are deduplicated by hash, which also stops gossip loops when the same
   change arrives over WebRTC and through a relay.

The engine (`crates/sync`) is sans-io: hosts feed it frames and time and carry
out the effects it returns. This is what makes the deterministic network
simulator possible.

### In the browser

- The WASM engine and IndexedDB run in a dedicated Web Worker.
  `RTCPeerConnection` lives on the main thread, and frames are handed across as
  transferable buffers.
- Browsers join a signaling room on each relay and keep a partial mesh of up to
  6 WebRTC peers. The peer with the lower id makes the offer. Frames are split
  into chunks of at most 16 KiB, and sending backs off when the DataChannel
  buffer fills.
- Tabs of one origin sync over `BroadcastChannel`. Only one tab owns the
  IndexedDB store (chosen with Web Locks). The others run in memory and that tab
  replicates their data.
- The WASM module is about 160 KiB gzipped.

## SDK reference (short)

`openDb(options)` options: `appId` (required), `relays`, `persistence`
(`"indexeddb"` | `"memory"`), `worker`, `webrtc`, `syncWithRelays`, `tabs`,
`maxPeers`, `iceServers`, `anonymous`, `wasm`.

- **Documents:** `collection`, `doc`, `setDoc(ref, data, {merge})`, `updateDoc`,
  `deleteDoc`, `addDoc`, `getDoc`, `getDocs`, `writeBatch`, `deleteField()`.
- **Queries:** `query(coll, where(f, op, v)…, orderBy(f, dir), limit(n))` with
  `== != < <= > >= in`. One `orderBy`. Only top-level fields.
- **Live:** `onSnapshot(refOrQuery, next, error)` gives `docChanges()` and
  `metadata.fromCache` (true until a peer has sent its backlog for that scope).
- **Auth:** `db.auth.userId`, `login(alias, passphrase)`, `createRandom()`,
  `loginWithSeed(seed)`, `exportSeed()`, `logout()`, `onChange(cb)`.
- **Spaces:** `db.spaces.create(name)`, `open(id)`, `invite(space, pubkey, role)`,
  `remove(space, pubkey)`, `members(space)`.
- **Network:** `db.network.status()`, `goOffline()`, `goOnline()`,
  `waitForSync(pathOrScope)`, `addLink(link)` for custom transports.

Values can be null, booleans, numbers, strings, `Uint8Array`, arrays and plain
objects. A `Date` is stored as milliseconds.

## Relay

```sh
aghanim-relay --listen 0.0.0.0:8787 --data-dir ./data      # any app id
aghanim-relay --config relay.toml                          # see relay.example.toml
docker build -t aghanim-relay . && docker run -p 8787:8787 -v aghanim:/data aghanim-relay
```

Endpoints: `GET /v1/<app>` (WebSocket), `GET /v1/<app>/ice` (STUN/TURN list for
browsers), `GET /health`. Configure a TURN server in `ice_servers` for peers
behind strict NATs. Without one, those peers still sync, through the relay.

## Testing

```sh
just test    # cargo test --workspace && pnpm -r test
just e2e     # Playwright: starts a relay and the demo dev server
just lint    # rustfmt, clippy (native + wasm32), biome, tsc
```

- **Rust:** unit tests, merge property tests (proptest), and crypto tests
  (RFC 8032 vector, tamper and forgery, AAD binding, key wrapping, a pinned
  passphrase vector). Also a deterministic simulator: 5 peers and a relay under
  random delay, dropped links, partitions and forged changes, over 40 seeds,
  asserting identical documents and no forged data. Relay integration tests use
  native clients over real WebSockets: sync, restart persistence, forged-change
  rejection, signaling auth, ICE config.
- **SDK (Vitest):** the compiled WASM engine in Node. Covers CRUD, queries,
  listeners, two peers over an in-memory link, spaces, IndexedDB persistence
  (fake-indexeddb), passphrase login, and DataChannel chunking.
- **E2E (Playwright, Chromium):**
  - A write reaches another browser over a direct WebRTC link (relay used for
    signaling only) in under 1 s.
  - Offline edits on both sides converge.
  - Writes to another user's space are rejected.
  - The relay's database file holds only ciphertext for space content.
  - Removing a member rotates the key.
  - Sync falls back to the relay when WebRTC can't connect.
  - The demo UI works end to end.

## Known limitations

- **The browser store loads into memory.** IndexedDB is read into memory at
  startup and writes are journaled back. Fine for app-sized data; an OPFS
  backend is the planned upgrade.
- **Revocation depends on clocks.** Writes are accepted from a removed member if
  they are stamped before the removal. A removed member could backdate writes
  (they still lose LWW against newer writes, and can't read anything encrypted
  after the rotation). A write that arrives before the grant that allows it is
  held in a bounded in-memory queue and is lost on restart.
- **Anyone can read any scope.** There are no read permissions: public data is
  public, and space content is ciphertext. Metadata is visible as described
  above.
- **Saved logins are stored unencrypted.** With `remember` (the default), the
  identity seed is stored in IndexedDB, like Gun's `recall`.
- **Signaling payloads aren't signed.** Only the join is bound to the
  authenticated device key. A malicious relay could disrupt WebRTC setup but
  can't impersonate a peer on a DataChannel, because the sync handshake
  authenticates both ends.
- **Queries are limited.** Only top-level fields, one `orderBy`, no composite
  indexes, no cursors (`startAfter`).

## Roadmap

Composite indexes and query cursors, CRDT field types (counters, text) using the
reserved CBOR tags, Merkle reconciliation for peers without a cursor, relay-free
peer discovery, an OPFS storage backend, TURN hosting, native mobile and desktop
SDKs, and proof-of-work or rate limits against spam in `public/`.

## License

aghanim-network is licensed under the GNU General Public License v3.0 or later.
See [LICENSE](LICENSE) for the full text.
