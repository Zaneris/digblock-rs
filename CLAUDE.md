# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Rust + wgpu rewrite of the DigBlock voxel engine (originally Java/libGDX, 2015–2017). The initial port matched the Java original faithfully (in places bit-exactly), but that goal is complete — the engine is now free to diverge and improve. The old Java repo at `../DigBlock` remains a useful behavior reference, not a constraint.

## Commands

```sh
just run                  # run desktop app (cargo run -p digblock-desktop)
just run --drill          # automated lifecycle drill (suspend/resume + device-loss)
just bench                # deterministic perf run: release build, scripted camera flight,
                          # inputs disabled, vsync-paced, CSV to bench.csv, summary line
                          # (fill time + frame-pacing counts) on exit; add --no-vsync for
                          # uncapped raw-cost percentiles; findings live in BENCH.md
just check                # full gate: cargo fmt --all --check, clippy -D warnings, cargo test --workspace
just check-ios            # cargo check -p digblock-ios --target aarch64-apple-ios (no Xcode needed)
just ios-sim              # simulator build+run (requires full Xcode)
just ios-device           # device build (requires full Xcode)
just server               # dedicated multiplayer server (headless; journal persists in ./data)
just bot -- --bots 4      # headless bot clients against a running server
just compose wifi         # dockerized server behind tc netem (clean | wifi | cellular)
just compose-bots wifi    # same + 4-bot soak, exits non-zero on missed snapshots
```

Multiplayer: `just play` (server + connected client, one command; closing the window stops both) or `just server` then `just run -- --connect https://127.0.0.1:4433` (×N windows). Use `127.0.0.1`, not `localhost` — macOS resolves `localhost` to `::1` and the server binds IPv4 only, so `localhost` times out. A desktop client can also play against the *containerized* server — `just play-docker [clean|wifi|cellular]` runs the compose server (netem shapes the real client) — but this needs Docker Desktop's "Use kernel networking for UDP" enabled (Settings → Resources → Network); the default user-space UDP proxy drops QUIC handshake replies (host→container works, container→host is eaten).

Run a single test: `cargo test -p digblock-core --test worldgen` (integration test file) or `cargo test -p digblock-core <test_name>`. CI (`.github/workflows/ci.yml`) runs the same steps as `just check` plus desktop and iOS builds; clippy warnings fail CI.

Deterministic-capture / metrics flags for `just run`: `--seed <f64>`, `--tod <deg>` (freezes the sun), `--screenshot out.png`, `--frames N`, `--exit`, `--metrics-csv out.csv`, `--overlay` (start with the debug overlay on; backquote toggles it at runtime), `--no-vsync` (uncapped present), `--workers N`, `--draw-dist N`, `--mobile` (draw dist 10, 1024px shadows — the iOS config).

Metrics (`digblock-core/src/metrics.rs`) collect per-phase CPU frame timings, GPU pass ms via timestamp queries (feature-detected), chunk-pipeline job timings/queue gauges, and renderer draw stats; everything lands in the 5s log line, the CSV, and the toggleable on-screen overlay (`digblock-render/src/overlay.rs`, zero cost when hidden). Frame-pacing diagnostics: the bench is vsync-paced by default (`--no-vsync` for uncapped raw cost), and `DIG_SPIKELOG=1` logs per-phase breakdowns of frames over the 120Hz budget. Main-thread streaming work (result drain + mesh uploads) is time-budgeted per frame (`STREAM_BUDGET_MS` in engine.rs) and the wide shadow cascade re-renders sliced across frames into a ping-ponged back map, so streaming throughput cannot hitch the render loop.

Formatting uses hard tabs, max width 100 (`rustfmt.toml`).

## Architecture

Workspace with six crates, two thin app launchers, and a dedicated server. The crate boundaries are load-bearing:

- **`crates/digblock-core`** — pure game logic (worldgen, chunks, meshing, fluid rules, player physics, touch/input driving, sun, metrics). No GPU or windowing deps; everything is testable headless.
- **`crates/digblock-platform`** — the winit shell (`app.rs`) around a pure lifecycle decision core (`lifecycle.rs` — a headless-tested state machine turning OS/drill/surface events into ordered shell actions), input translation, screenshot capture. This is the **only** crate allowed to touch `winit` or `wgpu::Surface`. It defines the `Game` trait and the `Gpu` struct (process-lifetime device/queue).
- **`crates/digblock-render`** — the engine: world state + wgpu renderer. Implements `Game`. It renders into `&TextureView`s borrowed for a single `render()` call and never sees the surface, so it cannot hold a stale swapchain texture across a suspend on any platform. Split into `engine.rs` (world state, streaming, frame orchestration) and `renderer.rs` (device-lifetime state + pass structure), with per-system modules: `shadow.rs` (two-cascade system incl. the wide ping-pong), `sky.rs`, `water.rs`, `spray.rs`, `players.rs`, `overlay.rs`, `pipeline_builder.rs`.
- **`crates/digblock-assets`** — compile-time embedded textures and WGSL shaders (`include_bytes!`/`include_str!`), so every platform gets byte-identical assets. Shaders are naga-validated by a test, so CI catches WGSL errors without a GPU.
- **`crates/digblock-net`** — multiplayer protocol + client netcode, **sans-IO** (no tokio/sockets — wasm-clean; a future browser client implements the same `NetTransport` trait over the WebTransport JS API). Wire format is postcard, append-only evolution, `PROTOCOL_VERSION` bumped on breaks. Holds the movement predictor (rewind/replay reconciliation), pending-edit tracking (authoritative-beats-pending precedence), remote-player interpolation, and an in-memory loopback transport that makes all of it plain-`cargo test`-able.
- **`crates/digblock-net-native`** — the native transport driver: wtransport (WebTransport/QUIC) on a detached background thread, bridged over non-blocking channels. Only `apps/desktop` and the server's bot link it; iOS is untouched.
- **`apps/desktop`**, **`apps/ios`** — thin launchers (arg parsing / staticlib entry).
- **`apps/server`** — headless dedicated server (lib + `digblock-server`/`bot` bins). Authoritative sim on one 60Hz thread (`sim.rs`, socket-free and fully tested by injected events), wtransport net layer (`net.rs`), multi-center no-mesh chunk store (`store.rs`), append-only edit journal with boot compaction (`journal.rs` — terrain is never sent; join sync = seed + edits).

### Multiplayer model

Server-authoritative everything, but movement uses an **adoption envelope**: clients send per-tick inputs *plus* their claimed post-tick state; the server runs the same `Player::tick` and adopts the claim when it's within `POS_TOLERANCE`, sanely displaced, and not inside geometry — so honest clients never accumulate drift and never rubber-band. A second, time-integrated gate closes the edge-riding exploit: the server keeps an EWMA of each adopted claim's (claim − sim) delta *vector* (`DRIFT_ALPHA`, ~2 s window); honest deltas are zero-mean float noise and cancel, while sustained directional bias trips `DRIFT_TOLERANCE` within ~6 ticks and suspends adoption until the average decays — capping cheat gain at ~1.2 blocks/s sustained (vs 4 walking). Out-of-envelope claims keep the server's state and the next snapshot corrects the client (rewind/replay, surfaced as a ~200ms decaying camera offset, never a snap). Dig/place is predicted locally, validated server-side by re-raycasting from the claimed origin (`EDIT_POS_TOLERANCE` vs the server's player position), and confirmed or reverted. Fluid runs only on the server; results broadcast as ordinary edits. Protocol constants live in `digblock-net/src/protocol.rs`. `DIG_PLAYERBOX=1` renders a fake remote player 5 blocks ahead (GPU-path check without a second client).

### Java heritage

The port was originally validated bit-exactly against the Java original; those parity tests have been retired, and behavior — including terrain produced by a given seed — has since diverged substantially (256-block world, shaped terrain, caves, trees, dig/place). Comments citing Java code explain provenance, not obligations.

### Blocks (core/block.rs)

Block behavior is data-driven and generated by the `blocks!` macro — one row per block produces the `repr(u16)` enum, `COUNT`/`ALL`, and the `PROPS` table together, so they cannot skew. Each row holds the block's `RenderClass` (None/Opaque/Cutout/Translucent), `collides`, per-face texture layers, and mesh bucket (`MeshBucket`); water is a block *family* (source, `WaterFlow1..7`, `WaterFall`) whose behavior is `FluidProps` data. Adding a block = one `blocks!` row + PNG + `LAYERS` entry in digblock-assets (layer order is append-only). Chunk cells are `u16` end-to-end — 10-bit block id plus spare bits, travelling the wire and the journal as-is (protocol v3, journal v2). The mesher, physics, fluid, and raycast all key off the tables — no per-block special cases elsewhere.

### Worldgen (core/worldgen/ + decoration.rs)

Everything is a pure function of `(x, z, seed)` — column content never depends on generation order. The module splits by stage: `terrain.rs` (control fields — continentalness picks base elevation through a spline, roughness gates where mountain relief appears, a ridge field draws the ranges, detail fBm adds local relief; all tuning lives in `TerrainParams`), `rivers.rs` (meandering channels from the zero band of a warped low-frequency field — U-profile beds, fading out in mountains, meeting the sea at grade), `caves.rs` (carve where two independent 3D-noise bands intersect, land columns only), and `classify.rs` (per-column `SurfaceKind` — Ocean/Beach/Plains/Mountain, the future-biome seam — with each kind's block rules as `SurfaceProps` table data). `SEA_LEVEL = 128` of a 256-block world. Trees run as a decoration pass (fill → carve → decorate) using pure apron evaluation: candidates come from a position hash, every chunk within `STRUCTURE_APRON` computes the identical tree independently, so structures span chunk borders with no pipeline state. Sky chunks resolve to the shared `AIR_CHUNK` sentinel Arc (never meshed, recognized by pointer).

### Chunk pipeline (core/pipeline.rs + mesher.rs)

A worker pool replaces the Java one-item-per-frame loop. The main thread owns the chunk store and scheduling; workers run pure functions (terrain gen, meshing) over immutable snapshots — a mesh job gets a padded 18³ copy of its chunk (a one-block shell from all 26 neighbors, feeding per-vertex AO and water corner heights), no shared mutation. Staleness is handled by generation counters: results whose `gen` doesn't match the slot's current counter are dropped. A newly generated chunk bumps its own and its Ready neighbors' counters — face neighbors unconditionally, diagonals only where the shared shell cells can matter. Block edits go through `Pipeline::set_block` (dig/place): copy-on-write via `Arc::make_mut`, bumping the chunk and only the neighbors whose shell contains the edit. Vertices are packed as two u32 words — word 0 geometry at half-block resolution, word 1 material with a 10-bit texture-layer field (`MAX_TEX_LAYERS` = 1024) and, for water, the surface/flow payload in the upper bits.

### Lifecycle robustness

The suspend/resume and device-loss paths are standing regression targets for a future Android port. `Renderer::new` is a pure constructor so device-loss recovery is "build a new one and re-upload meshes" (`Game::rebuild_gpu_resources`). `Game::on_suspend` must be cheap and synchronous — on iOS/Android the process may be killed with no further callbacks. Test via `just run --drill` (automated) or F9/F10 at runtime; F8 takes a screenshot.

## iOS notes

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` — edit the yml and rerun `xcodegen`, never edit the project file directly. Simulator/device builds need full Xcode (Command Line Tools alone are not enough); `just check-ios` is the fast compile gate that needs no Xcode.
