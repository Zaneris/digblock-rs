# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Recipes live in the `justfile`; `just --list` shows them all. The essentials:

```sh
just check                # the gate: fmt, dprint check-md, clippy -D warnings, fleet unit tests, workspace tests (needs Docker)
just test <filter>        # cargo test --workspace --no-fail-fast; filter narrows to matching tests
cargo test -p <crate> <name>   # one test in one crate
just check-ios            # iOS compile gate (cargo check for aarch64-apple-ios, no Xcode needed)
just check-web            # wasm32 clippy + every shader through real Chrome's WGSL front end (needs Chrome)
just web-e2e <title>      # browser smoke suite (Playwright); title regex narrows to one test
just gate                 # check + check-ios + check-web + web-e2e, i.e. "am I done"
just run                  # desktop client, debug build
just web                  # wasm build served at http://127.0.0.1:8080 (Chrome/WebGPU)
just play                 # containerized master + native server + client, all local, torn down together
just bench                # deterministic perf run (release build; window must stay visible or it stalls)
just fmt-md               # dprint re-wrap of Markdown (this file included) to 120 cols
```

`just check` needs Docker running: the server e2e tests boot the real `apps/master` .NET container. CI is intentionally
disabled pre-deployment; the just gates are the gate.

Workspace lints deny all warnings and `unreachable_pub`, so plain `cargo build` surfaces what CI would. Rust uses hard
tabs, `max_width = 100` (rustfmt.toml).

**Running test suites requires asking first**, exactly like committing: never launch `just check`, `just test`,
`just gate`, `just web-e2e`, `cargo test`, or any other test run without the user's go-ahead. Fast static checks are
always fine unprompted: `cargo fmt`, `cargo clippy`, `cargo check`, `dprint`. The one exception: when executing a large
multi-step plan that involves committing along the way, run the tests before each of those commits as you go.

## Comment style

- File-top comments: 15 lines max.
- Function/method comments: 5 lines max.
- Enum member comments: 1 line max.
- No em dashes in player-visible game strings; use a comma, colon, or hyphen instead.

## Versioning and compatibility

- `PROTOCOL_VERSION` / `MIN_PROTOCOL` (`crates/digblock-net/src/protocol.rs`): bump when the wire protocol is no longer
  backward compatible. Wire types use postcard with positional discriminants, so enum variants and struct fields are
  append-only: never reorder, never remove. The 8-byte preamble is frozen forever and never versioned.
- `SIM_VERSION` (`crates/digblock-core/src/lib.rs`): covers what client and server compute independently and must agree
  on bit-for-bit (`Player::tick`, dig tables, the body of every shipped terrain generator). Adding a new
  `TerrainVersion` row needs no bump; editing an existing row in place does.
- Journal/persistence (`crates/digblock-persist/src/journal.rs` `VERSION`, and inventory/player-save serialization):
  changes must always be attempted backward compatible. A hard break requires explicit approval, because it breaks
  existing worlds and player saves. A journal `VERSION` bump refuses old worlds but leaves them standing; starting fresh
  is an explicit operator call (`archive_world`), never a boot side effect.
- The two save schemes model how to evolve without breaking: the server's per-account inventory store
  (`apps/server/src/player_store.rs`) keeps every version it has ever written loadable, using frozen `FIRST_*_VERSION`
  markers to trigger on-read migrations, and refuses only files newer than itself. Client local-save content migrations
  are the `REV_*` constants / `SAVE_REV` in `crates/digblock-persist/src/lib.rs`, keyed on the version, never on the
  contents.

## Architecture

Rust + wgpu voxel game: one engine crate driving macOS desktop, Chrome (WebGPU/wasm), and iOS, plus a headless dedicated
server and a .NET master (accounts, server registry, join tokens; `apps/master`, outside the cargo workspace). The
README's Layout table maps crates to roles; what follows is the cross-crate contracts.

Platform priority: **the web build is the focus** — web desktop and web mobile are the main way the game is played.
Desktop native is supported but less important. iOS is not currently supported as a shipping platform, but must keep
compiling (`just check-ios` stays in the gate).

### Platform / engine seam

`digblock_platform::Game` is the trait the engine (`digblock-client`) implements. **Only `digblock-platform` may touch
`winit` or `wgpu::Surface`**; the engine receives a `&wgpu::TextureView` borrowed for a single `render()` call.
Lifecycle (suspend/resume/device-loss) is a pure state machine in `crates/digblock-platform/src/lifecycle.rs`,
unit-testable headless. `on_suspend` must be cheap and synchronous; `on_exit` is the only callback allowed to block;
never rely on `Drop` on wasm.

Launchers (`apps/desktop`, `apps/ios`, `apps/web`) are thin: they own platform services and inject them into the engine
via setters in `crates/digblock-client/src/engine/setup.rs` (transport, menu HTTP services, audio sink, settings store,
clipboard, job dispatch, world store). The engine never does HTTP and never sees the session token.

### Time

Use `web_time::Instant`, never `std::time::Instant`, in any crate that compiles to wasm (`std::time::Instant` is fine in
`apps/server` and native-only modules; `std::time::Duration` is fine everywhere).

### Netcode

`digblock-net` is sans-IO and wasm-clean: no sockets, no async runtime. The seam is `transport::NetTransport` (all
methods sync and non-blocking), implemented by `digblock-net-native` (wtransport/QUIC on a detached thread) and
`digblock-net-web` (hand-written WebTransport bindings), with an in-memory loopback for tests. The trait is `Send`
off-wasm and deliberately not `Send` on wasm32. `NetClient` owns prediction, predicted edits, and interpolation, and
hands the engine plain `NetEvent`s; the engine never touches wire bytes.

The protocol is edits + snapshots, never terrain: both sides generate the world from the seed (which is why
`SIM_VERSION` must match exactly). Joins sync via journal columns; player state rides unreliable datagrams. Wire types
use plain arrays, never glam types.

### Server, persistence, master

`apps/server` runs one authoritative fixed-tick `Sim` on one thread, socket-free by construction: `net.rs` feeds it
`SimEvent`s and it emits pre-encoded `OutMsg`s onto bounded per-client channels (a full channel marks the client dead
rather than buffering). Everything is testable with plain `cargo test`; `bin/netbench.rs` drives the real Sim through
the same seam.

`digblock-persist` is an append-only journal of fixed 16-byte records, last-write-wins. The engine-facing `WorldStore`
trait is synchronous and must degrade (log, keep running) rather than fail the game. Server world journal and client
local saves share the format.

There is no unregistered server mode and no tokenless join: the server registers with the master (retrying forever), the
master probes it over real QUIC, and clients join only by presenting a master-minted token the server verifies back. The
server port range is mirrored across the C# master, deploy files, and the fleet tool, and the test
`the_masters_port_block_mirrors_ours` reads those files to keep them honest.

### Core pipeline (chunks, meshing, jobs)

`digblock-core` is pure logic: no GPU, windowing, sockets, filesystem, or async. The chunk pipeline
(`crates/digblock-core/src/pipeline.rs`) runs workers as pure functions over immutable snapshots; staleness rides
generation counters, never flags, and stale results are dropped. Executor arms: inline, native threads, or an injected
`JobDispatch` (how the wasm build fans out to `apps/web-worker`, a second wasm module exposing only `run_job_bytes`).
`JobDispatch::reset()` has a hard contract: after it returns, no result for a previously submitted job may ever be
delivered.

`TerrainVersion` (`crates/digblock-core/src/worldgen/mod.rs`) lists every generator that ever shipped, in order, none
ever removed; a terrain change adds a row rather than editing one. The golden test `a_shipped_generator_never_moves`
pins a hash per row that may only move alongside a `SIM_VERSION` bump.

### Flags and knobs

`crates/digblock-client/src/engine/knobs.rs` holds `knobs::ALL`, the single table behind desktop `DIG_*` env vars and
web `?name=` query params. A knob existing on one platform but not the other is impossible by construction. A malformed
flag or knob value is fatal (exit/panic), never a silent default, everywhere: launcher flags, knobs, server args. Config
precedence: CLI/query overrides > saved settings > platform defaults.

### Assets and shaders

Everything in `digblock-assets` is embedded at compile time. Generated textures come from `cargo run -p texgen` (never
hand-edit; outputs are committed, and one run legitimately rewrites every noisy texture since recipes share one RNG).
WGSL shared preludes are prepended via `concat!` in a fixed order; all render-pipeline descriptors funnel through
`crates/digblock-client/src/pipeline_builder.rs`. Shader validation is three non-overlapping gates: naga parse
(`digblock-assets` test), full wgpu-core validation with `Backends::NOOP` (`every_pipeline_builds_without_a_gpu` in the
renderer), and real Chrome's Tint via `just check-shaders`; Tint rejects WGSL naga accepts, so passing native gates is
not enough for web.

### iOS

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` via `xcodegen`; edit the yml, never the project.

## Tests

Unit tests live beside the code as sibling `mod tests;` files (or focused `*_tests.rs` files under
`crates/digblock-client/src/engine/`). Integration tests: `crates/digblock-core/tests/worldgen.rs` (goldens),
`crates/digblock-net/tests/loopback.rs` (full client/server netcode with no sockets), `apps/server/tests/e2e.rs` (boots
the real master container; Docker required). Cross-language constants are pinned by tests that read the other side's
files from disk, so a mirror edit shows up as a test failure, not a drift.
