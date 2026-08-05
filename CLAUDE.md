# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Rust + wgpu rewrite of the DigBlock voxel engine (originally Java/libGDX, 2015–2017). The faithful-port goal is complete; the engine is free to diverge and improve. The old Java repo at `../DigBlock` is a behavior reference, not a constraint.

## Commands

```sh
just run                  # run desktop app (cargo run -p digblock-desktop)
just run --drill          # automated lifecycle drill (suspend/resume + device-loss)
just bench                # deterministic perf run: release, scripted flight, CSV + summary;
                          # --no-vsync for uncapped raw-cost percentiles; findings in BENCH.md
just check                # full gate: fmt --check, clippy -D warnings, cargo test --workspace
                          # (needs Docker: server e2e tests boot the apps/master container)
just check-ios            # cargo check for aarch64-apple-ios (no Xcode needed)
just web                  # wasm build + serve at http://localhost:8080 (Chrome/WebGPU; needs
                          # wasm-bindgen-cli at Cargo.lock's exact version)
just check-web            # clippy for wasm32-unknown-unknown — no CLI tools needed
just play                 # master container + native server + client at the menu
just play-web             # same but serving the web bundle; open the printed URL
just play-docker wifi     # desktop client vs containerized stack (clean | wifi | cellular netem)
just ios-sim / ios-device # build+run via Xcode (full Xcode required)
just master               # .NET master natively (QUIC probe needs `brew install libmsquic`)
just server               # dedicated server (journal in ./data; --master-url defaults to the
                          # production master https://master.digblock.net; joining is token-only)
just bot -- --bots 4      # headless bots: pick a server via the master, mint tokens, join
just compose wifi         # dockerized master + server behind tc netem
just compose-bots wifi    # same + 4-bot soak, exits non-zero on missed snapshots
just profile              # release-speed build with debug symbols
```

Multiplayer is master-first with **no direct-connect path**: the .NET master (`apps/master`) lists servers, distributes cert pins, and mints one-time join tokens; a game server refuses any Hello it can't verify with the master. Display names are mandatory (3-10 chars of `a-z A-Z 0-9 - _`; canonical `validate_name` in `digblock-net/src/protocol.rs`, mirrored as a regex backstop in the master's `/api/join`). Chat: T/Enter to type, server sanitizes to printable ASCII and rate-limits (`CHAT_*` in protocol.rs); remote players get overlay name tags. Dev networking gotchas: use `127.0.0.1`, never `localhost` (macOS resolves it to `::1`; the server binds IPv4 only); the containerized master probes the host via `probeHost = host.docker.internal` (a LAN-IP hairpin breaks QUIC address validation under Docker's NAT — production servers omit `probeHost`); if a client times out against the docker stack, read the server container's logs before blaming the network — a server dead at boot looks identical. Extra windows: `just run -- --master-url http://127.0.0.1:5200`, add `--join --name <name>` for scripted flows.

Run a single test: `cargo test -p digblock-core --test worldgen` or `cargo test -p digblock-core <test_name>`. CI is intentionally disabled pre-deployment (`.github/workflows/ci.yml.disabled`); `just check` + `just check-ios` are the gate.

Base images/toolchains in CI and Docker files are **suite-pinned on purpose** (builder and runtime must share a glibc; floating `1-slim` once broke a release). Review and bump the pins in `docker/Dockerfile*`, `.github/workflows/*`, and `apps/master/Dockerfile` deliberately — together where the comments say so — with a test release.

Capture/metrics flags for `just run`: `--seed <f64>`, `--tod <deg>` (freezes the sun; dev builds only — in multiplayer the server dictates time of day), `--screenshot out.png`, `--frames N`, `--exit`, `--metrics-csv`, `--overlay` (backquote toggles), `--no-vsync`, `--workers N`, `--draw-dist N`, `--mobile` (the iOS config). Metrics (`digblock-core/src/metrics.rs`) land in the 5s log line, the CSV, and the overlay; `DIG_SPIKELOG=1` logs per-phase breakdowns of over-budget frames. Streaming work is time-budgeted per frame (`STREAM_BUDGET_MS`) and the wide shadow cascade re-renders sliced across frames, so streaming can't hitch the render loop.

Formatting uses hard tabs, max width 100 (`rustfmt.toml`).

### Settings & local worlds

The in-game Settings screen exposes draw distance (runtime-applied; connected clients send `ClientMsg::SetDrawDist` so the server's sync radius follows), Music/SFX volumes, and an SSR toggle. Persistence is a launcher-injected `SettingsStore` (desktop: `data_dir/digblock/settings.json`; web: `localStorage`); precedence is CLI/query > saved > platform default, and non-interactive runs (bench/screenshot/drill) skip load AND save so captures stay hermetic. The last-joined multiplayer name persists here too. `Settings` (digblock-render/src/settings.rs) is serde-free; each launcher owns a serde mirror.

Named local worlds live in `crates/digblock-persist`: the journal (shared `VERSION` — a worldgen retune stales server volumes and client saves together) plus the `WorldStore` trait. Desktop `FsWorldStore` writes the exact server-format journal + state.json with throttled atomic writes; web `LocalStorageWorldStore` uses localStorage (not IndexedDB) because `flush` must run inside synchronous `on_suspend`. Version policy: client saves are **refused, never archived** on mismatch (delete-only in the UI); the `.vN.bak` archive path is server-only. World UI is in `menu.rs`; leaving a server swaps to a fresh backdrop and relocks the startup menu. `Pipeline`'s optional `EditOverlay` records every `set_block` and re-applies cells before air classification, so local edits survive hard eviction even in unsaved worlds (restored water re-schedules fluid).

### Audio

The engine emits `AudioCmd`s into an injected `Box<dyn AudioSink>` (a `None` sink covers bench/screenshot/drill/iOS). Pure helpers: `digblock-render/src/audio.rs` (materials, footsteps, ambient beds) and `water_audio.rs` (waterfall voice reconciliation). Fillers: `apps/desktop/src/audio.rs` (rodio; positional coordinates scaled by `ATT = 0.08` to shape rodio's fixed 1/d rolloff) and `apps/web/src/audio.rs` (WebAudio; one-time pointerdown/keydown `resume()` for Chrome's autoplay rule). SFX are embedded OGGs (`digblock-assets::sfx`, provenance in `assets/sfx/SOURCES.md`). Music is never embedded: the master serves CC0 tracks (`/api/music`) and both fillers fetch at runtime. Mute: `--mute` / `?mute`.

### Browser client

Chrome via wasm32-unknown-unknown + WebGPU (`apps/web`). Query params mirror desktop flags: `?seed= ?tod= ?draw= ?ssr=0 ?shadow= ?overlay ?name= ?mute ?master= ?join&server=`; defaults are full desktop quality; `?master=` defaults to the production master, explicitly empty disables multiplayer + music. Key mechanics: chunk jobs run inline on the main thread (`workers: 0`); `std::time::Instant` is `web_time::Instant` workspace-wide; GPU init is async (`WaitingForGpu` state); the depth "copy" is a blit into R32Float (WebGPU forbids depth32float as a copy destination); shadow taps use `textureSampleCompareLevel` (Tint's derivative-uniformity rule). Multiplayer: `crates/digblock-net-web` implements `NetTransport` over WebTransport with `serverCertificateHashes` from the master; the server's self-signed cert is 10-day validity (Chrome caps pinned certs at 14 days) and rotates in-process every 7, re-registering so the master hands out the fresh pin. `apps/web/src/services.rs` is the fetch()-based `MenuServices` filler. Scripted joins go through `EngineConfig::auto_join`, which walks the real menu path. Screenshots, bench, drill, and metrics CSV are native-only.

## Architecture

Workspace with eight crates, two thin app launchers, and a dedicated server. The crate boundaries are load-bearing:

- **`crates/digblock-core`** — pure game logic (worldgen, chunks, meshing, fluid, player physics, input driving, sun, metrics). No GPU or windowing deps; everything testable headless.
- **`crates/digblock-platform`** — the winit shell around a headless-tested lifecycle state machine (`lifecycle.rs`), input translation, screenshot capture. The **only** crate allowed to touch `winit` or `wgpu::Surface`. Defines the `Game` trait and `Gpu` (process-lifetime device/queue).
- **`crates/digblock-render`** — the engine: world state + wgpu renderer, implements `Game`. Renders into `&TextureView`s borrowed per `render()` call and never sees the surface (so it can't hold a stale swapchain texture across a suspend). `engine.rs` (state/streaming/orchestration) + `renderer.rs` (device-lifetime state, passes) + per-system modules (`shadow.rs`, `sky.rs`, `water.rs`, `spray.rs`, `players.rs`, `overlay.rs`, `menu.rs`/`chat.rs`, `audio.rs`/`water_audio.rs`, `ui/`).
- **`crates/digblock-assets`** — compile-time embedded textures and WGSL (byte-identical on every platform). Shaders are naga-validated by a test, so WGSL errors surface without a GPU.
- **`crates/digblock-net`** — protocol + client netcode, **sans-IO** (wasm-clean). Postcard wire format, append-only evolution, `PROTOCOL_VERSION` bumped on breaks. Holds the movement predictor, pending-edit tracking, remote-player interpolation, and a loopback transport for plain `cargo test`.
- **`crates/digblock-net-native`** — wtransport (WebTransport/QUIC) on a background thread, bridged over non-blocking channels. Only desktop and the bot link it.
- **`crates/digblock-persist`** — persistence shared by server and client saves: journal format (index/codec/snapshot wasm-clean; file writer + compaction cfg'd native), `WorldStore` trait, `FsWorldStore`. Sits above core AND net. Web store impl lives in `apps/web/src/store.rs`.
- **`apps/desktop`**, **`apps/ios`** — thin launchers. Desktop also owns the menu's HTTP services (`services.rs`); the engine only sees `MenuServices` channels.
- **`apps/server`** — headless dedicated server. Authoritative 60Hz sim (`sim/`, socket-free, tested by injected events), wtransport layer (`net.rs` — verifies join tokens with the master before the sim hears about a connection), master HTTP client (`master.rs`), chunk store, append-only edit journal with boot compaction (terrain is never sent; join sync = seed + edits; old-version journals archive to `.vN.bak` — this destructive path is server-only).
- **`apps/master`** — .NET 10 minimal API (BCL only, not in the cargo workspace). In-memory registry + one-time token store; verifies reachability with a mandatory QUIC probe pinned to the registered cert hash (no probe-off mode; hosts without UDP egress or libmsquic fail at boot). The Rust e2e tests boot its real container. Never put `Urls` in appsettings.json (it overrides `ASPNETCORE_URLS` and breaks the container bind). The HTTP contract in `Contracts.cs` is mirrored by `apps/server/src/master.rs` and `apps/desktop/src/services.rs` — change all three together.

### Multiplayer model

Joining is token-only: client gets a one-time token from the master (`POST /api/join`), sends it in `Hello`, the server consumes it via `POST /api/tokens/verify` (the master's display name wins). Verification fails **closed**; connected players are unaffected by a master outage.

Server-authoritative everything, but movement uses an **adoption envelope**: clients send per-tick inputs plus claimed post-tick state; the server runs the same `Player::tick` and adopts claims within `POS_TOLERANCE` that aren't inside geometry — honest clients never drift or rubber-band. A time-integrated EWMA of claim−sim deltas (`DRIFT_ALPHA`/`DRIFT_TOLERANCE`) suspends adoption under sustained directional bias, closing the edge-riding exploit. Rejected claims are corrected by the next snapshot via rewind/replay, surfaced as a ~200ms decaying camera offset, never a snap. Dig/place is predicted locally and validated server-side by re-raycasting from the claimed origin. Fluid runs only on the server; results broadcast as ordinary edits. Protocol constants live in `digblock-net/src/protocol.rs`. `DIG_PLAYERBOX=1` renders a fake remote player for GPU-path checks.

### Blocks (core/block.rs)

Block behavior is data-driven via the `blocks!` macro — one row produces the `repr(u16)` enum, `COUNT`/`ALL`, and the `PROPS` table together, so they can't skew. Rows hold `RenderClass` (None/Opaque/Cutout/Translucent/Shaped), `collides`, per-face texture layers, and `MeshBucket`; water is a block family driven by `FluidProps` data. Adding a block = one row + PNG + `LAYERS` entry (append-only); derived textures come from the checked-in `tools/texgen`, outputs committed. Chunk cells are `u16` end-to-end: 10-bit id, spare bits 10-11, STATE nibble in 12-15, travelling wire and journal as-is (protocol v8, journal v3). Mesher, physics, fluid, and raycast all key off the tables — no per-block special cases elsewhere.

### Sub-block shapes (core/shapes.rs) + alchemy

Shaped blocks live on a **12×12×12 sub-grid** (12 divides by 2/3/4/6), geometry STATIC per (block id, state nibble) — no per-instance micro-voxels. `shapes.rs` authors each shape as sub-box lists in a canonical +x facing and bakes every variant at first use (rotate → rasterize → greedy-merge; pure integer, deterministic) into render quad templates, boundary coverage masks (`FaceMask`, the culling currency), collision boxes, and interaction boxes. Per-shape geometry (stair boxes, door leaf/handle/hinge, the door's rigid `swing_open` rotation) lives in `shapes.rs` comments and pinned tests — don't re-document it here. `state_valid` is THE wire gate for state bits; `Facing` is axis-named (`Dir::West` is +x — compass names are a trap).

Mesher: shaped cells stamp templates via `emit_shaped` with flat ao=3 (shaped blocks neither cast nor receive AO); boundary quads cull against `neighbor_cover`; `MAX_SHAPE_QUADS` (test-pinned per variant) sizes the renderer's `MAX_QUADS`. Vertices pack positions in twelfths (8 bits/axis); shaders decode with an integer whole/remainder split so lattice points stay exact under GPU fast-math — chunk/water/shadow.wgsl must always agree.

Physics (SIM v8): stairs collide as a **diagonal ramp** (`RAMP_SNAP_*` constants; beyond the band or pinched under a low ceiling they revert to their boxes and block like walls; the ramp run-out lets a flight mount the flush cube it tops out against, mirrored in `overlaps_solid` so mid-mount claims stay adoptable). Open doors have no collision. `overlaps_solid`/`placement_blocked` are the shape-aware occupancy contract shared by client place-rejection, the server's InsidePlayer scan, and the adoption envelope's probe — standing on a shaped block is NOT "inside solid". Raycasts narrow-phase against collision boxes (miss → DDA continues); `raycast_interact` uses interaction boxes so open doors stay clickable.

Right-click is use-beats-place: an interactable under the ray toggles via `EditKind::Interact`, which rides the whole existing EditRequest validation/rate-limit/queue machinery (the server flips both door halves). Multi-cell shapes place/dig as a pair written lower-first in one tick, client-predicted under one seq via `PendingEdits::begin_multi`; per-cell LWW journal replay keeps pairs consistent. v1 limits (documented, deliberate): shaped blocks float when support is dug, fluid treats them as solid, water emits no face against them.

Acquisition is the **alchemy screen** (K): convert blocks → sub-material units (1 block = 27; sub-materials ride every wire/save format as flagged ids `0x8000|family`) and craft shaped items at `craft_recipe` costs — filled volume rounded up to whole 64-twelfths³ units; fractional-volume shapes must use batch recipes. `ClientMsg::Alchemy` is NOT client-predicted (the server answers every op with an `InventoryFull` rebase) and rides the edit action queue so craft-then-place in one tick debits in wire order; offline runs the same `apply_alchemy`. `DIG_SHAPES=1` places a showcase of every variant near spawn.

### Worldgen (core/worldgen/ + decoration.rs)

Everything is a pure function of `(x, z, seed)` — column content never depends on generation order. Stages: `terrain.rs` (continentalness/roughness/ridge/detail control fields, tuning in `TerrainParams`, terraced erosion), `rivers.rs` (channels from the zero band of a warped field, meeting the sea at grade), `lakes.rs` (sparse bowls in flat inland country; lake beats river per column), `caves.rs` (intersection of two 3D-noise bands, land only), `classify.rs` (per-column `SurfaceKind` + climate biomes, block rules/tree density/species as `SurfaceProps` table data; climate shifts the lines and picks the biome, never the terrain). `SEA_LEVEL = 128` of a 256-block world. Structures are a decoration pass using pure apron evaluation: per-family salted position hashes (`structures_near`), every chunk within `STRUCTURE_APRON` computes identical structures independently, so they span borders with no pipeline state; trunk roots are the single exception to decoration's write-into-Air-only rule. Sky chunks resolve to the shared `AIR_CHUNK` sentinel Arc (never meshed, recognized by pointer).

### Chunk pipeline (core/pipeline.rs + mesher.rs)

The main thread owns the chunk store and scheduling; workers run pure functions (terrain gen, meshing) over immutable snapshots — a mesh job gets a padded 18³ copy of its chunk (one-block shell from all 26 neighbors, feeding AO and water corner heights). Staleness is handled by generation counters: results whose `gen` doesn't match are dropped; a new chunk bumps its own and affected Ready neighbors' counters. Edits go through `Pipeline::set_block`/`set_cell` (copy-on-write via `Arc::make_mut`, bumping only neighbors whose shell contains the edit). Vertices are two u32 words: word 0 geometry (positions in twelfths), word 1 material (10-bit texture layer, `MAX_TEX_LAYERS` = 1024; water carries its surface/flow payload in the upper bits).

### Lifecycle robustness

Suspend/resume and device-loss are standing regression targets for a future Android port. `Renderer::new` is a pure constructor so device-loss recovery is "build a new one and re-upload meshes" (`Game::rebuild_gpu_resources`). `Game::on_suspend` must be cheap and synchronous — the process may be killed with no further callbacks. Test via `just run --drill` or F9/F10 at runtime; F8 screenshots.

## iOS notes

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` — edit the yml and rerun `xcodegen`, never the project file. Simulator/device builds need full Xcode; `just check-ios` is the fast gate that doesn't.
