# DigBlock (Rust)

Rust + wgpu voxel game, a rewrite of the DigBlock engine originally built in Java/libGDX (2015–2017). The port started
as a faithful recreation and is now free to diverge: probe-traced global illumination, sub-block shapes, two skills over
a crafting economy, and account-authenticated multiplayer.

Runs on macOS, in Chrome via WebGPU, and on iOS, off one engine crate. A headless dedicated server and a .NET master
(accounts, server listing, join tokens) sit behind the multiplayer; for now the master is out of the loop and clients
join one server directly with a per-device key.

## Commands

```sh
just run                  # desktop client
just run --drill          # lifecycle drill (suspend/resume + device-loss)
just web                  # wasm build, served at http://localhost:8080 (Chrome/WebGPU)
just play                 # master + dedicated server + client, all local
just play-web             # same, serving the web bundle
just server               # dedicated server alone
just master               # .NET master alone
just bot -- --bots 4      # headless bots: pick a server, mint tokens, join
just bench                # deterministic perf run — scripted flight, CSV + summary
just bench -- --stare     # the GI worst case: still stares down from the top of the world, per-station summary (--pitch/--yaw re-aim it)
just check                # the gate: fmt, clippy -D warnings, workspace tests (needs Docker)
just check-ios            # iOS compile gate (no Xcode needed)
just check-web            # wasm clippy + every shader through real Chrome's WGSL front end
```

`just check` and `just check-ios` are the gate; CI is intentionally disabled pre-deployment.

## Controls

**WASD** move · mouse look · **Space** jump · **F** walk/fly · **I** inventory · **K** alchemy · **C** character · **T**
chat · **9** cycle tools · **`** debug overlay · **Esc** menu. Touch builds get an anchored joystick, drag look, and an
on-screen HUD.

Debug keys: **F7** re-queue GI · **F8** screenshot · **F9** suspend/resume drill · **F10** device-loss drill.

## Layout

| Crate                                  | What it is                                                                                                            |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `crates/digblock-core`                 | Pure logic — worldgen, chunks, meshing, fluid, physics, skills, economy. No GPU or windowing deps; testable headless. |
| `crates/digblock-client`               | The client: wgpu renderer, GI, UI, menus, audio, and the engine that drives them.                                     |
| `crates/digblock-platform`             | The winit shell and lifecycle state machine. The only crate that may touch `winit` or `wgpu::Surface`.                |
| `crates/digblock-assets`               | Compile-time embedded textures, WGSL and fonts.                                                                       |
| `crates/digblock-net`                  | Protocol and client netcode, sans-IO and wasm-clean.                                                                  |
| `crates/digblock-net-native` / `-web`  | WebTransport/QUIC transports for desktop and browser.                                                                 |
| `crates/digblock-persist`              | Journal format and world stores, shared by server and client saves.                                                   |
| `apps/desktop`, `apps/ios`, `apps/web` | Thin launchers.                                                                                                       |
| `apps/server`                          | Headless dedicated server: authoritative fixed-tick sim, append-only edit journal.                                    |
| `apps/master`                          | .NET 10 minimal API — accounts, server registry, join tokens. Not in the cargo workspace.                             |
| `tools/texgen`, `tools/shadercheck`    | Derived-texture generator; the real-Chrome shader gate.                                                               |

## iOS

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` — edit the yml and rerun `xcodegen` rather than
editing the project. Simulator and device builds need **full Xcode**; `just check-ios` is the fast gate that does not.
