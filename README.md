# DigBlock (Rust)

Rust + wgpu rewrite of the DigBlock voxel engine (originally Java/libGDX,
2015–2017). The port started as a faithful recreation — same terrain,
water, day/night sun, and dual "Shadow LoD" cascades — and is now free to
diverge and improve from there. Modern internals: threaded chunk pipeline,
WGSL shaders on Metal, and a lifecycle-robust platform shell designed so a
future Android target inherits no surface/lifecycle bugs.

## Commands

```sh
just run                  # desktop (macOS)
just run --drill          # automated lifecycle drill (suspend/resume + device-loss)
just bench                # deterministic perf run (release; scripted flight, inputs off, vsync off)
just check                # fmt + clippy + tests
just check-ios            # iOS compile gate (no Xcode needed)
just ios-sim              # simulator build+run   (requires full Xcode)
just ios-device           # device build          (requires full Xcode)
```

Deterministic-capture / metrics flags:
`--seed <f64>` `--tod <deg>` (freezes the sun) `--screenshot out.png`
`--frames N` `--exit` `--metrics-csv out.csv` (per-phase CPU ms, GPU pass
ms, pipeline job timings — one row per 5s) `--overlay` (start with the
debug overlay visible) `--bench` (scripted camera flight, inputs disabled,
1800 frames, CSV to bench.csv, one-line summary on exit — incl. world
fill time and over-8.3ms/over-12.5ms frame-pacing counts; vsync-paced
so it looks as played — add `--no-vsync` for uncapped raw-cost
percentiles) `--no-vsync` (uncapped present) `--workers N`
`--draw-dist N` (workload knobs) `--mobile` (draw dist 10, 1024px
shadows — the iOS config). Env: `DIG_SPIKELOG=1` logs a per-phase
breakdown of every frame over the 120Hz budget.

Controls: **WASD/arrows** move · mouse look (click to capture cursor) ·
**Space** jump · **F** toggle walk/fly · **`** (backquote) debug overlay ·
**Esc** release cursor / quit.
Touch (iOS): left half = anchored joystick, right half = drag look,
double-tap right = jump.

Debug keys (desktop): **F8** screenshot · **F9** suspend/resume drill ·
**F10** device-loss drill.

## iOS

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` —
edit the yml and rerun `xcodegen` (brew) rather than editing the project.
Building for simulator/device requires **full Xcode** (Command Line Tools
alone are not enough): install from the App Store, then
`sudo xcode-select -s /Applications/Xcode.app`. Signing: automatic,
personal team.

## Layout

- `crates/digblock-core` — pure logic (terrain, chunks, player, metrics); no GPU deps
- `crates/digblock-render` — wgpu renderer; never touches winit or the surface
- `crates/digblock-platform` — winit shell, lifecycle state machine, input translation
- `crates/digblock-assets` — compile-time embedded textures + WGSL (naga-validated in CI)
- `apps/desktop`, `apps/ios` — thin launchers

The old Java repo (`../DigBlock`) is a historical reference; it still runs
under JDK 8 for side-by-side screenshot comparison.
