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
```

Run a single test: `cargo test -p digblock-core --test worldgen` (integration test file) or `cargo test -p digblock-core <test_name>`. CI (`.github/workflows/ci.yml`) runs the same steps as `just check` plus desktop and iOS builds; clippy warnings fail CI.

Deterministic-capture / metrics flags for `just run`: `--seed <f64>`, `--tod <deg>` (freezes the sun), `--screenshot out.png`, `--frames N`, `--exit`, `--metrics-csv out.csv`, `--overlay` (start with the debug overlay on; backquote toggles it at runtime), `--no-vsync` (uncapped present), `--workers N`, `--draw-dist N`, `--mobile` (draw dist 10, 1024px shadows — the iOS config).

Metrics (`digblock-core/src/metrics.rs`) collect per-phase CPU frame timings, GPU pass ms via timestamp queries (feature-detected), chunk-pipeline job timings/queue gauges, and renderer draw stats; everything lands in the 5s log line, the CSV, and the toggleable on-screen overlay (`digblock-render/src/overlay.rs`, zero cost when hidden). Frame-pacing diagnostics: the bench is vsync-paced by default (`--no-vsync` for uncapped raw cost), and `DIG_SPIKELOG=1` logs per-phase breakdowns of frames over the 120Hz budget. Main-thread streaming work (result drain + mesh uploads) is time-budgeted per frame (`STREAM_BUDGET_MS` in engine.rs) and the wide shadow cascade re-renders sliced across frames into a ping-ponged back map, so streaming throughput cannot hitch the render loop.

Formatting uses hard tabs, max width 100 (`rustfmt.toml`).

## Architecture

Workspace with four crates and two thin app launchers. The crate boundaries are load-bearing:

- **`crates/digblock-core`** — pure game logic (worldgen, chunks, meshing, player physics, touch/input driving, sun, metrics). No GPU or windowing deps; everything is testable headless.
- **`crates/digblock-platform`** — winit shell, lifecycle state machine, input translation, screenshot capture. This is the **only** crate allowed to touch `winit` or `wgpu::Surface`. It defines the `Game` trait and the `Gpu` struct (process-lifetime device/queue).
- **`crates/digblock-render`** — the engine: world state + wgpu renderer. Implements `Game`. It renders into `&TextureView`s borrowed for a single `render()` call and never sees the surface, so it cannot hold a stale swapchain texture across a suspend on any platform.
- **`crates/digblock-assets`** — compile-time embedded textures and WGSL shaders (`include_bytes!`/`include_str!`), so every platform gets byte-identical assets. Shaders are naga-validated by a test, so CI catches WGSL errors without a GPU.
- **`apps/desktop`**, **`apps/ios`** — thin launchers (arg parsing / staticlib entry).

### Java heritage

The port was originally validated bit-exactly against the Java original; those parity tests have been retired, and behavior — including terrain produced by a given seed — is free to diverge and improve. Some numeric oddities (e.g. f32 multiplies widened to f64 in `worldgen.rs`) are leftovers from reproducing Java arithmetic and can be simplified when touching that code. Comments citing Java code explain provenance, not obligations.

### Chunk pipeline (core/pipeline.rs + mesher.rs)

A worker pool replaces the Java one-item-per-frame loop. The main thread owns the chunk store and scheduling; workers run pure functions (terrain gen, meshing) over immutable snapshots — a mesh job gets its own chunk plus six copied border planes, no shared mutation. Staleness is handled by generation counters: results whose `gen` doesn't match the slot's current counter are dropped. A newly generated chunk bumps its own and all six Ready neighbors' counters.

### Lifecycle robustness

The suspend/resume and device-loss paths are standing regression targets for a future Android port. `Renderer::new` is a pure constructor so device-loss recovery is "build a new one and re-upload meshes" (`Game::rebuild_gpu_resources`). `Game::on_suspend` must be cheap and synchronous — on iOS/Android the process may be killed with no further callbacks. Test via `just run --drill` (automated) or F9/F10 at runtime; F8 takes a screenshot.

## iOS notes

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` — edit the yml and rerun `xcodegen`, never edit the project file directly. Simulator/device builds need full Xcode (Command Line Tools alone are not enough); `just check-ios` is the fast compile gate that needs no Xcode.
