export interface Example {
  id: string;
  title: string;
  summary: string;
  code: string;
}

// Scripts run as the body of an async function. In scope: every export of
// @aghanim/sdk, the peers `alice` and `bob`, `link`, `log`, `sleep`, `until`.
export const examples: Example[] = [
  {
    id: "live-sync",
    title: "Live sync",
    summary: "Bob listens to a collection while Alice writes to it.",
    code: `// Two peers, \`alice\` and \`bob\`, each run their own engine and store.
// They are connected by an in-memory link, so nothing leaves this page.
const notes = (db) => collection(db, "public/playground/notes");

onSnapshot(query(notes(bob), orderBy("at")), (snap) => {
  for (const change of snap.docChanges()) {
    log(\`bob: \${change.type}\`, change.doc.data());
  }
});

await addDoc(notes(alice), { text: "Hello from Alice", at: Date.now() });
await sleep(400);
const ref = await addDoc(notes(alice), { text: "A second note", at: Date.now() });
await sleep(400);
await updateDoc(ref, { text: "A second note, edited" });
await sleep(400);
await deleteDoc(ref);
`,
  },
  {
    id: "queries",
    title: "Queries",
    summary: "where, orderBy and limit, kept live with onSnapshot.",
    code: `const scores = (db) => collection(db, "public/playground/scores");

// A batch commits several writes at once, signed as one change.
const batch = writeBatch(alice);
for (const [name, points] of [["ana", 42], ["ben", 17], ["cy", 88], ["dee", 63], ["eli", 5]]) {
  batch.set(doc(scores(alice), name), { name, points });
}
await batch.commit();

const top3 = query(scores(bob), where("points", ">=", 20), orderBy("points", "desc"), limit(3));
onSnapshot(top3, (snap) => {
  if (snap.empty) return;
  log("bob's top 3:", snap.docs.map((d) => \`\${d.id} \${d.get("points")}\`).join(", "));
});

await sleep(500);
log("alice gives ben 99 points");
await updateDoc(doc(scores(alice), "ben"), { points: 99 });
await sleep(500);
log("alice removes cy's points field");
await updateDoc(doc(scores(alice), "cy"), { points: deleteField() });
`,
  },
  {
    id: "offline",
    title: "Offline edits",
    summary: "Cut the link, edit on both sides, and watch them converge.",
    code: `const card = (db) => doc(db, "public/playground/cards/c1");
const show = async (who, db) => log(\`\${who}:\`, (await getDoc(card(db))).data());

await setDoc(card(alice), { title: "Draft", color: "blue", votes: 0 });
await until(async () => (await getDoc(card(bob))).exists());
await show("bob", bob);

link.cut();
log("link cut: both peers keep reading and writing locally");
await updateDoc(card(alice), { title: "Alice's title", votes: 1 });
await updateDoc(card(bob), { color: "green" });
await sleep(10);
await updateDoc(card(bob), { title: "Bob's title (written last)" });
await show("alice", alice);
await show("bob", bob);

link.restore();
log("link restored: each field keeps its latest write");
await until(async () => (await getDoc(card(alice))).get("color") === "green");
await show("alice", alice);
await show("bob", bob);
`,
  },
  {
    id: "permissions",
    title: "Write permissions",
    summary: "users/{id} is writable only by its owner; every peer enforces it.",
    code: `const me = alice.auth.userId;
log("alice's user id (her public key):", me);

const profile = (db) => doc(db, \`users/\${me}/profile/main\`);
await setDoc(profile(alice), { name: "Alice", bio: "Writes signed changes" });

try {
  await setDoc(profile(bob), { name: "Mallory" });
  log("bob's write went through?!");
} catch (e) {
  log("bob was rejected:", e.message);
}

await until(async () => (await getDoc(profile(bob))).exists());
log("bob can still read it:", (await getDoc(profile(bob))).data());
`,
  },
  {
    id: "spaces",
    title: "Encrypted spaces",
    summary: "End-to-end encrypted data, invites, and key rotation on removal.",
    code: `const space = await alice.spaces.create("secret notes");
log("alice created", \`spaces/\${space.id}\`);
await alice.spaces.invite(space, bob.auth.userId, "writer");

const note = (db, id) => doc(db, \`spaces/\${space.id}/notes/\${id}\`);
await setDoc(note(alice, "n1"), { text: "Only members can read this" });

await bob.spaces.open(space.id);
const n1 = await until(async () => {
  const s = await getDoc(note(bob, "n1"));
  return s.exists() && s;
});
log("bob decrypts n1:", n1.data());
// Peers and relays store and forward this signed, encrypted record.
log("raw record (tag 40100 is ciphertext):", await rawDoc(bob, note(bob, "n1").path));

await setDoc(note(bob, "n2"), { text: "Bob can write too" });
await until(async () => (await getDoc(note(alice, "n2"))).exists());
const members = await alice.spaces.members(space);
log("members:", members.map((m) => \`\${m.role} \${m.pubkey.slice(0, 8)}\`).join(", "));

log("alice removes bob, which rotates the space key");
await alice.spaces.remove(space, bob.auth.userId);
await setDoc(note(alice, "n3"), { text: "Written after Bob left" });
await sleep(500);
log("bob reads n3:", (await getDoc(note(bob, "n3"))).data() ?? "nothing: he doesn't have the new key");
`,
  },
  {
    id: "identity",
    title: "Identity",
    summary: "An alias and passphrase give the same identity on every device.",
    code: `log("anonymous ids:", alice.auth.userId.slice(0, 16), bob.auth.userId.slice(0, 16));

// The keypair is derived with Argon2id, so it takes a moment.
const a = await alice.auth.login("ada", "correct horse battery staple", { remember: false });
const b = await bob.auth.login("ada", "correct horse battery staple", { remember: false });
log(a === b ? "same user on both peers:" : "different users?!", a);
log("device ids still differ:", alice.deviceId.slice(0, 16), bob.deviceId.slice(0, 16));

// Both devices can now write to the shared user's own scope.
await setDoc(doc(bob, \`users/\${a}/settings/theme\`), { mode: "dark" });
await until(async () => (await getDoc(doc(alice, \`users/\${a}/settings/theme\`))).exists());
log("alice sees bob's write:", (await getDoc(doc(alice, \`users/\${a}/settings/theme\`))).data());
`,
  },
];
