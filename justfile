# aghanim-network tasks. `just --list` shows them all.

default:
    @just --list

# Build the WASM engine and every package.
build:
    node scripts/build-wasm.mjs
    cargo build --workspace --release

# Rust and TypeScript tests (unit, property, simulation, relay integration).
test:
    cargo test --workspace
    pnpm -r test

# Browser end-to-end tests (starts a relay and the demo's dev server).
e2e:
    pnpm exec playwright test -c tests/e2e/playwright.config.ts

# Lints and format checks.
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p aghanim-wasm --target wasm32-unknown-unknown -- -D warnings
    pnpm exec biome check .
    pnpm -r typecheck

# Run a local relay. Extra arguments go to aghanim-relay.
relay *args:
    cargo run -p aghanim-relay --release -- {{args}}

# Run the demo against a local relay (start `just relay` first).
demo:
    pnpm --filter @aghanim/demo dev

# Run the SDK playground (two in-page peers, no relay needed).
playground:
    pnpm --filter @aghanim/playground dev
