# DigBlock (Rust)

Rust + wgpu rewrite of the DigBlock voxel engine (originally Java/libGDX,
2015–2017). Faithful port — same terrain, water, day/night sun, and dual
"Shadow LoD" cascades — with modern internals: threaded chunk pipeline,
WGSL shaders on Metal, and a lifecycle-robust platform shell designed so a
future Android target inherits no surface/lifecycle bugs.

## Commands

```sh
just run                  # desktop (macOS)
just run --drill          # automated lifecycle drill (suspend/resume + device-loss)
just run --screenshot shots/x.png --exit
just check                # fmt + clippy + tests
just check-ios            # iOS compile gate (no Xcode needed)
just ios-sim              # simulator build+run   (requires full Xcode)
just ios-device           # device build          (requires full Xcode)
```

Debug keys (desktop): **F8** screenshot · **F9** suspend/resume drill ·
**F10** device-loss drill · **Esc** release cursor / quit.

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

The old Java repo (`../DigBlock`) is the behavior reference; it still runs
under JDK 8 for side-by-side screenshot comparison.
