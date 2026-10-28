import * as sdk from "@aghanim/sdk";
import { javascript } from "@codemirror/lang-javascript";
import { Compartment, Prec } from "@codemirror/state";
import { oneDark } from "@codemirror/theme-one-dark";
import { keymap } from "@codemirror/view";
import { basicSetup, EditorView } from "codemirror";
import { type Example, examples } from "./examples.ts";
import { format } from "./format.ts";
import { createPeers, type Peers } from "./peers.ts";
import { $ } from "./shared/app.ts";
import "./style.css";

// --- output -----------------------------------------------------------------
type Level = "log" | "warn" | "error" | "info";

const output = $<HTMLOListElement>("#output");
let runStarted = performance.now();
// Bumped on every run, so callbacks left over from an earlier run stay quiet.
let generation = 0;

function print(level: Level, args: unknown[]): void {
  const li = document.createElement("li");
  li.className = level;
  const time = document.createElement("span");
  time.className = "time";
  time.textContent = `${Math.round(performance.now() - runStarted)}ms`;
  const text = document.createElement("span");
  text.className = "text";
  text.textContent = args.map(format).join(" ");
  li.append(time, text);
  const atBottom = output.scrollHeight - output.scrollTop - output.clientHeight < 24;
  output.append(li);
  if (atBottom) output.scrollTop = output.scrollHeight;
}

// Mirror the page console (including the SDK's own warnings) into the panel.
for (const level of ["log", "info", "warn", "error"] as const) {
  const original = console[level].bind(console);
  console[level] = (...args: unknown[]) => {
    original(...args);
    print(level, args);
  };
}
window.addEventListener("unhandledrejection", (e) => print("error", [e.reason]));

// --- peers ------------------------------------------------------------------
let peers: Peers | undefined;
const linkButton = $<HTMLButtonElement>("#link");

function showLink(up: boolean): void {
  linkButton.dataset.up = String(up);
  linkButton.textContent = up ? "linked" : "cut";
  linkButton.title = up ? "Cut the link between the peers" : "Restore the link";
}

function showPeer(name: "alice" | "bob", db: sdk.Db | undefined): void {
  const el = $(`#peer-${name} code`);
  const id = db?.auth.userId;
  el.textContent = id ? `${id.slice(0, 10)}…` : "…";
  el.title = id ?? "";
}

async function resetPeers(): Promise<Peers> {
  const old = peers;
  peers = undefined;
  linkButton.disabled = true;
  await old?.close();
  const next = await createPeers(showLink);
  peers = next;
  for (const name of ["alice", "bob"] as const) {
    showPeer(name, next[name]);
    next[name].auth.onChange(() => showPeer(name, next[name]));
  }
  linkButton.disabled = false;
  return next;
}

linkButton.onclick = () => {
  if (!peers) return;
  if (peers.link.up) peers.link.cut();
  else peers.link.restore();
};

// --- editor -----------------------------------------------------------------
const drafts = new Map<string, string>();
let current: Example = examples[0];

const dark = window.matchMedia("(prefers-color-scheme: dark)");
const theme = new Compartment();
const editor = new EditorView({
  parent: $("#editor"),
  extensions: [
    basicSetup,
    javascript(),
    theme.of(dark.matches ? oneDark : []),
    Prec.highest(
      keymap.of([
        {
          key: "Mod-Enter",
          run: () => {
            void run();
            return true;
          },
        },
      ]),
    ),
    EditorView.updateListener.of((u) => {
      if (!u.docChanged) return;
      const code = u.state.doc.toString();
      if (code === current.code) drafts.delete(current.id);
      else drafts.set(current.id, code);
      $<HTMLButtonElement>("#revert").hidden = !drafts.has(current.id);
    }),
  ],
});
dark.addEventListener("change", () => editor.dispatch({ effects: theme.reconfigure(dark.matches ? oneDark : []) }));

function select(example: Example): void {
  current = example;
  history.replaceState(null, "", `#${example.id}`);
  $("#title").textContent = example.title;
  $("#summary").textContent = example.summary;
  for (const a of document.querySelectorAll<HTMLAnchorElement>("#examples a")) {
    a.toggleAttribute("aria-current", a.dataset.id === example.id);
  }
  const code = drafts.get(example.id) ?? example.code;
  editor.dispatch({ changes: { from: 0, to: editor.state.doc.length, insert: code } });
}

$("#examples").append(
  ...examples.map((ex) => {
    const a = document.createElement("a");
    a.href = `#${ex.id}`;
    a.dataset.id = ex.id;
    a.innerHTML = "<strong></strong><span></span>";
    a.querySelector("strong")!.textContent = ex.title;
    a.querySelector("span")!.textContent = ex.summary;
    a.onclick = (e) => {
      e.preventDefault();
      select(ex);
    };
    return a;
  }),
);

$<HTMLButtonElement>("#revert").onclick = () => {
  drafts.delete(current.id);
  select(current);
};

// --- running scripts ----------------------------------------------------------
const AsyncFunction = Object.getPrototypeOf(async () => {}).constructor as new (
  ...args: string[]
) => (...args: unknown[]) => Promise<unknown>;

const runButton = $<HTMLButtonElement>("#run");

async function run(): Promise<void> {
  if (runButton.disabled) return;
  runButton.disabled = true;
  const gen = ++generation;
  output.replaceChildren();
  runStarted = performance.now();

  try {
    print("info", ["Starting two fresh peers…"]);
    const { alice, bob, link } = await resetPeers();
    const live = () => gen === generation;
    const scope: Record<string, unknown> = {
      ...sdk,
      alice,
      bob,
      link,
      log: (...args: unknown[]) => live() && print("log", args),
      sleep: (ms: number) => new Promise((r) => setTimeout(r, ms)),
      async until<T>(check: () => T | Promise<T>, timeoutMs = 5000): Promise<T> {
        const end = Date.now() + timeoutMs;
        for (;;) {
          const v = await check();
          if (v) return v;
          if (Date.now() > end) throw new Error(`until() timed out after ${timeoutMs}ms`);
          await new Promise((r) => setTimeout(r, 20));
        }
      },
    };
    runStarted = performance.now();
    const script = new AsyncFunction(...Object.keys(scope), editor.state.doc.toString());
    runButton.disabled = false;
    await script(...Object.values(scope));
    if (live()) print("info", ["Script finished. Listeners stay live until the next run."]);
  } catch (e) {
    if (gen === generation) print("error", [e]);
  } finally {
    if (gen === generation) runButton.disabled = false;
  }
}

runButton.onclick = () => void run();
$<HTMLButtonElement>("#clear").onclick = () => output.replaceChildren();

select(examples.find((ex) => ex.id === location.hash.slice(1)) ?? examples[0]);
void run();
