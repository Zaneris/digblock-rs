# Building a 3D engine for the browser: notes from DigBlock

A working document. It records how this engine reaches Chrome (desktop and mobile) and why each part is shaped the way
it is, so that someone building a different Rust + wgpu engine for the web can start from what already cost us time
instead of rediscovering it. Every claim below points at the file that holds the mechanism; the comments in those files
carry the fuller reasoning.

Scope: the web build is the focus of this project. Desktop native is supported and iOS must keep compiling, but the
decisions here were made for a browser first, and several of them only make sense in that light.

## 1. The shape of the engine

One engine crate drives every platform. The launchers are thin, and the seams between them are traits with documented
contracts, which is what lets the same code run under winit on macOS, under a canvas in Chrome, and under UIKit.

| Crate                                 | Role                                                                                                         |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `crates/digblock-core`                | Pure logic: worldgen, chunks, meshing, physics. No GPU, windowing, sockets, filesystem, or async.            |
| `crates/digblock-client`              | The engine: wgpu renderer, GI, UI, audio commands, streaming, settings.                                      |
| `crates/digblock-platform`            | The winit shell and the lifecycle state machine. The only crate allowed to touch `winit` or `wgpu::Surface`. |
| `crates/digblock-assets`              | Textures, WGSL and fonts, all embedded at compile time.                                                      |
| `crates/digblock-net`                 | Protocol and client netcode. Sans-IO, wasm-clean.                                                            |
| `crates/digblock-net-native` / `-web` | The two transports: wtransport/QUIC on a thread, and hand-written WebTransport bindings.                     |
| `crates/digblock-persist`             | The journal format and world stores shared by the server and client saves.                                   |
| `apps/web`, `apps/web-worker`         | The browser launcher and the worker-side wasm module.                                                        |

Three rules shape everything that follows:

- **Injection at the launcher, never in the engine.** `crates/digblock-client/src/engine/setup.rs` exposes setters for
  the transport, HTTP menu services, audio sink, settings store, clipboard, job dispatch, and world store. The engine
  never does HTTP and never sees a session token. The web launcher (`apps/web/src/lib.rs`) is where browser APIs live.
- **Degrade, never fail.** Audio degrades to silence, settings to per-session, the worker pool to inline execution, the
  world store to playing from memory. A browser tab lives for hours, so nothing may be a one-way door.
- **The wasm rules are workspace-wide.** `web_time::Instant`, never `std::time::Instant`, in any crate that compiles to
  wasm. `std::time::Duration` is fine everywhere. No `getrandom` (the default world seed is clock-derived to keep it out
  of the tree). `fontdue`'s `parallel` feature is never enabled because it pulls rayon and breaks the wasm build. Both
  web crates are `cfg(target_arch = "wasm32")`-gated end to end, so on the host they compile as empty rlibs and
  `cargo clippy --workspace` still covers them without pulling wasm-bindgen into a native build.

### The platform/engine seam

`digblock_platform::Game` (`crates/digblock-platform/src/lib.rs`) is the trait the engine implements. The one design
decision that makes suspend/resume tractable on every platform:

```rust
/// The game renders into the borrowed swapchain view; every other
/// target (depth, offscreen scene) is game-owned.
fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView, size: (u32, u32));
```

The engine receives a `&TextureView` borrowed for exactly one call, so it can never hold a stale swapchain texture
across a suspend. The `Gpu` bundle (instance, adapter, device, queue) is process-lifetime and survives suspend; the
surface is shell-owned and invisible to the engine.

Other contracts on the trait that matter for the web:

- `on_suspend()` must be cheap and synchronous. On a phone it may be the last callback the process ever gets.
- `on_exit()` is the only callback allowed to block, and it is deliberately not a `Drop` impl: destructors do not
  reliably run on wasm.
- `frame_started()` / `frame_acquired()` are two halves because main-thread occupancy is measured differently: natively
  the vsync wait blocks inside the acquire, while on the web the idle sits between frames. Deriving busy time from the
  frame-to-frame delta reads as "always full" on the web.
- `take_fullscreen_request()`, `text_entry_active()`, `text_replace()` and friends are drained right after each input
  dispatch, because the Fullscreen API and the soft keyboard only honour a call from inside the gesture still on the
  stack.

## 2. Building for the web

There is no trunk, no wasm-pack, no wasm-opt, no bundler, no `build.rs` for the web crates, and no `.cargo/config.toml`.
The whole pipeline is `cargo build --target wasm32-unknown-unknown --release`, the `wasm-bindgen` CLI, `sed`, and `cp`.
The recipe is `build-web` in the `justfile`.

The wasm target is stock stable, single-threaded linear memory. No `+atomics`, no shared memory, no COOP/COEP headers.
That is not an omission: multithreading is done with independent worker instances (section 7), which need none of it.

### Two modules, two stamps

The build produces two wasm modules: the main engine (`apps/web`) and a core-only worker module (`apps/web-worker`).
Each is paired with its wasm-bindgen glue, and each pair is content-addressed:

```bash
stamp=$( { cat "$wasm"; echo "$want"; } | sha256 | cut -c1-12)
```

The bindgen version is part of the hash because it decides the mangled import names. Glue and wasm are one artifact: a
stale `.js` against a fresh `.wasm` dies with a `LinkError` about an import that no longer exists. The two pairs get
separate stamps so an unchanged worker is not re-downloaded when the client moves.

`index.html` is the only file that knows either stamp. The build rewrites it with `sed` and publishes the worker URL
through one global that the Rust side reads back with `js_sys::Reflect`:

```js
globalThis.__digblockWorkerUrl = "./pkg/digblock_worker.js";
```

`build-web` also refuses to run if the installed `wasm-bindgen` CLI does not match the version in `Cargo.lock`, and
prints the exact install command. The deploy workflow pins the same version and must be bumped alongside the lockfile.

### Cache correctness

This was the most expensive lesson in the build. The `_headers` file for Cloudflare Pages reaches the Pages origin, but
the CDN in front of the custom domain rewrites `Cache-Control` for `.js` (a default static extension) and not for
`.wasm`. Observed live:

```
digblock.pages.dev/pkg/digblock_web.js  ->  no-cache
digblock.net/pkg/digblock_web.js        ->  max-age=14400
```

So headers alone cannot keep a pair coherent. The fix is the stamps above: every file under `/pkg/` is immutable by
name, and `apps/web/_headers` asserts that for a year. A stale `index.html` can only ask for a pair that no longer
exists, which is a clean 404 that heals on reload, never half of a live pair.

Two more holes had to be closed:

- The header rule matches the path, so it also decorated whatever Pages served for a missing file. Without a 404 page,
  that was `index.html` as a 200, and the edge cached HTML under a stamped `.js` name for a year. The module fetch is
  keyed apart from a plain GET by its `Origin` header, so `curl` said "fine" while Chrome got `text/html`.
  `apps/web/public/404.html` makes the miss an honest 404.
- `index.html` retries a failed pair once under a query-string salt (`?r=<base36>`), which is a fresh cache key at every
  layer, and re-publishes the worker URL under the same salt. `fetchWasm` also throws explicitly on a `text/html`
  content type.

### Loading in the page

`apps/web/index.html` is hand-written, with one inline script and one module script. The load sequence:

- `navigator.gpu` is checked before the module is downloaded, so a browser that cannot run it does not fetch it.
- Two `requestAnimationFrame` awaits happen before anything heavy. Without them the overlay never painted and the page
  stayed black until the first engine frame.
- The glue is loaded with a dynamic `import()`, because a static import that fails fires nothing the page can catch and
  cannot be retried.
- The wasm is hand-fetched so a progress bar can follow `Content-Length`, then re-wrapped in a `Response` with the
  original headers so wasm-bindgen still takes its `instantiateStreaming` path. `application/wasm` is mandatory for that
  path, and both dev servers set it explicitly.
- A single-shot failure handler is bound to `error`, `unhandledrejection`, and a `digblock-fatal` custom event. It
  prints the build stamp, the stage reached, seconds elapsed, whether the load was cache-busted, the navigation type,
  user agent, viewport, DPR, cores, device memory, and a three-way WebGPU probe: no WebGPU, no adapter (hardware
  acceleration off or blocklisted), or a named adapter that failed further in.
- A 30 second watchdog reports "Still <stage>" with a Reload button. It is aimed at iOS, which discards a heavy page
  under memory pressure and re-navigates on return, so a player who only rotated the phone may be looking at a fresh
  cold load.

The page and the platform crate share a fixed contract: the canvas id `digblock-canvas`, the worker URL global, and four
window events (`digblock-gpu-ready`, `digblock-first-frame`, `digblock-exit`, `digblock-fatal`). The e2e harness
installs its listeners before the page script runs so none can be missed.

### Serving

- **Production** is Cloudflare Pages, pure static files. The deploy workflow runs `just build-web` itself, so the bundle
  cannot skew from a local build.
- **Dev** is `tools/serve-web.py`, stdlib only. It serves http on `127.0.0.1:8080` and https on the LAN IP at 8443,
  because WebGPU needs a secure context and a phone reaching the machine by LAN IP is not on loopback. The self-signed
  cert persists in `data/dev-web-cert` and is regenerated only when the LAN IP leaves its SANs, so the phone's one-time
  trust survives runs. It also reverse-proxies `/master/*` so an https page is not blocked by mixed content when it
  talks to a plain-http local master.
- Always open `127.0.0.1`, never `localhost`. A loopback literal is a secure context, and the master allows a loopback
  sign-in redirect by IP literal only.

### Runtime configuration

The URL is the CLI. `apps/web/src/lib.rs` parses `window.location.search` into the engine config with two rules: an
unknown key is fatal, and a value that does not parse is fatal. `?vualt` would otherwise photograph open terrain as the
vault. Every knob comes from one table, `knobs::ALL` in `crates/digblock-client/src/engine/knobs.rs`, which the desktop
launcher reads from `DIG_*` environment variables and the web launcher reads from query params through the same `apply`
function. A knob existing on one platform but not the other is impossible by construction.

Config precedence is `URL > saved settings > platform defaults`, and capture paths (`?capture`, `?bench`) neither read
nor write settings.

## 3. The page shell and mobile browser hacks

Everything in this section lives in `apps/web/index.html` unless noted. The whole shell is about 530 lines with no
external CSS or JS.

### Viewport and canvas sizing

```html
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover, maximum-scale=1, user-scalable=no">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
```

```css
html, body { margin: 0; height: 100dvh; background: #000; overscroll-behavior: none; }
html { overflow: hidden; }
#digblock-canvas {
  position: fixed; top: 0; left: 0;
  width: 100%; height: 100dvh;
  touch-action: none;
  -webkit-touch-callout: none; user-select: none;
}
```

Why each line is there:

- `viewport-fit=cover` claims the notch area; safe-area insets are read back separately (below).
- `maximum-scale=1` and `user-scalable=no` suppress double-tap zoom heuristics. iOS ignores them for the accessibility
  pinch, so `touch-action: none` is the real gesture guard. winit's `prevent_default` covers pointer events, not the
  browser's own pinch and double-tap recognizers.
- `100dvh` is the dynamic viewport height. Plain `100%` is undersized under a mobile URL bar at load. The canvas is
  sized in `dvh` rather than `%` because a fixed box measures the layout viewport, which on iOS is the toolbar-hidden
  height, so a percentage hangs the bottom of the HUD behind the browser chrome.
- `overscroll-behavior: none` stops pull-to-refresh and rubber-banding under the game.
- The canvas is `position: fixed` because the document underneath is allowed to scroll for the toolbar hack, and the
  game must not travel with it.
- A `gesturestart` listener calls `preventDefault`, because WebKit's non-standard pinch events slip past `touch-action`
  on some iOS versions.

The page never sets `canvas.width` or `canvas.height`. winit's web backend installs a `ResizeObserver` with
`box: "device-pixel-content-box"` and sizes the backing store from `devicePixelContentBoxSize`, falling back to the
content rect times the scale factor on Safari. The e2e suite pins that the backing store equals the CSS box times the
device pixel ratio.

### Fullscreen and rotation

iPhone Safari has no Fullscreen API under either spelling. Everything else follows from that.

- `fullscreen_available()` in `crates/digblock-platform/src/app.rs` checks `document.fullscreenEnabled` and then the
  `webkit` prefix. The toggle uses whichever half answered, on both sides of the toggle, so the question and the action
  cannot disagree. A browser that answers the prefixed probe also reports its state under prefixed names, and asking the
  standard ones there reads as "not fullscreen" forever.
- The toggle runs inside the user gesture's own dispatch: the engine sets a request, and the shell drains it right after
  the input event that set it.
- Entering fullscreen also attempts `screen.orientation.lock("landscape")`, deferred by one browser yield because a lock
  requested before the fullscreen transition applies is refused even where it is supported. Android Chrome honours it
  once fullscreen; everyone else rejects, and the rejection is the answer, so both calls are fire-and-forget.
- On iPhone the menu renders a dim row where Fullscreen would be: "Fullscreen: Add to Home Screen (Share menu)". That is
  the sanctioned escape from browser chrome there. The installed-app state is detected under two spellings, the standard
  `(display-mode: standalone)` media query and Safari's `navigator.standalone`.
- Rotation reaches Rust only as a winit `Resized`, which reconfigures the surface. Safe-area insets are read once at
  boot, on the reasoning that the common mobile posture is one orientation per session, so a rotation costs the HUD its
  inset margin until reload. `orientationchange` is listened to in exactly one place, the toolbar hack below.

### Reclaiming the Safari toolbar in a tab

iOS Safari collapses its toolbar for a real touch scroll and for nothing else. Script cannot ask for it, `minimal-ui`
has been gone since iOS 8, and an inner scroll container was reported to work on iOS 15 and does not on current phones.
What it wants is the document scrolling. The mechanism:

- A 1px wide, `220dvh` tall, `pointer-events: none` spacer exists solely to make the document overflow. Without
  something overflowing, the gesture scrolls nothing and the browser never hears about it.
- `html.barswipe` turns document scrolling on. A visible handle (`#barswipe`, `touch-action: pan-y`, the only element
  allowed to scroll) sits below the safe-area top and clear of both corners, because the touch HUD's menu button owns
  one corner and the look-drag area is worth more than the handle. It is absolutely positioned, not fixed, so it travels
  with the scroll it starts; a swipe that does nothing visible is a swipe a player stops making.
- Eligibility is probed, never sniffed from the user agent: a hidden `height: 100lvh` element measures the large
  viewport, and `lvh - innerHeight > 8` means chrome is currently taking space. It is inert on desktop (equal) and in an
  installed web app (no chrome). The probe replaces a constant because the toolbar's size varies by device, by
  orientation, and by which of iOS 26's three toolbar layouts the player chose.
- The feedback loop: on `touchend`, if the page scrolled less than 24px the player is telling us it is in the way, so
  the feature gives up. Otherwise it waits 450ms and re-probes; a swipe that scrolled but did not move the chrome also
  gives up, because an offer that cannot be taken is worse than no offer. When it works, only the handle is hidden. The
  page stays scrollable and stays scrolled, because winding back to the top is exactly the gesture that brings the
  toolbar out again.
- Listeners arm 400ms after `digblock-first-frame`, not at script evaluation. A cold load on a phone sheds its toolbar
  while the wasm is still downloading, which fires `resize` behind the overlay and paints the handle over it; a tap on
  it scrolls nothing and gives up, and the feature is off for good before the player has seen the game.
- `resize`, `orientationchange` (with a 300ms settle), and `visualViewport.resize` all funnel into the same reconcile,
  which never runs while a finger is down, because taking the scroller away mid-gesture cancels the scroll being
  measured.

### Device pixel ratio

- Chromium's DPR emulation is only skin deep. `devicePixelRatio` reports the emulated ratio, but a `ResizeObserver`'s
  `devicePixelContentBoxSize` keeps answering in the host display's device pixels, and that is the box winit sizes the
  canvas from. The e2e harness (`tools/web-e2e/lib/digblock.mjs`) patches the getter to derive the device box from the
  CSS box times the ratio the page believes, and offers `--native` to drop emulation entirely, which is the only way to
  tell an emulation artifact from a bug.
- Wheel deltas arrive from winit's web backend pre-multiplied by DPR, which made one click scroll several hotbar slots.
  `crates/digblock-platform/src/input.rs` divides by the scale factor and by 100 pixels per notch, then clamps.
- A freshly created canvas reports 0x0 until its `ResizeObserver` fires. An async GPU init that beats the callback
  configures at the 1px floor, and the `Resized` that follows is dropped because winit only sends one on a change. Every
  async GPU hand-off re-reads `window.inner_size()` and reconfigures if it disagrees.
- UI scale is `quantize(scale_factor * window_factor) * user_preference`. Only the derived half is quantized to quarter
  steps, because it shimmers as a window is dragged otherwise. The world renders at `surface * render_scale`, and GI
  resolves at an integer division of the render size; UI is never render-scaled.

### The loading overlay

- The overlay is faded on `digblock-first-frame` and removed by a 600ms timer, not `transitionend`, because early inline
  chunk streaming can starve the animation timeline for seconds and freeze the fade mid-flight.
- The progress sweep animates `transform`, not `margin`. A margin animation needs the main thread every frame, and
  engine init holds that for seconds on a cold phone. A transform animation runs on the compositor.
- `@media (pointer: coarse)` bumps the overlay's font and bar sizes; desktop sizes read as a speck at arm's length.

## 4. Mobile input

### Detecting a phone

```rust
fn is_mobile_browser() -> bool {
    // A coarse primary pointer AND a touch digitizer: phones and tablets,
    // but not touchscreen laptops, whose primary pointer stays fine.
    navigator.maxTouchPoints > 0 && matchMedia("(pointer: coarse)").matches
}
```

Any API failure answers false, because desktop defaults are the safe fallback. `?mobile` and `?mobile=0` override in
both directions, which is also how a desktop A/Bs the mobile profile without a device in hand. The mobile profile in
`crates/digblock-client/src/engine/config.rs` lowers draw distance, disables screen-space reflections, raises the GI
resolve divisor, and enables the touch HUD.

### Pointer lock

A touch device has no pointer to lock, and iOS Safari lacks the API entirely. The single gate in `app.rs` refuses grabs
when running in touch mode; releases stay allowed. On the web, the per-frame cursor-grab reconcile is suppressed on its
rising edge, because a lock request from a `requestAnimationFrame` callback carries no user gesture and Chrome rejects
it, so re-grabs happen only from click and key handlers. Escape is excluded from those, because Chrome excludes it from
user activation and the failed lock would bounce the player straight back into the pause menu. Browser truth comes from
page-lifetime `pointerlockchange` and `pointerlockerror` listeners posted into winit as user events; winit's `Ok` from
`set_cursor_grab` does not mean the lock happened.

One more winit gotcha: `listen_device_events(DeviceEvents::Always)` is required on wasm, because the web backend drops
device events (mouse look) whenever the canvas lacks DOM focus, and the text bridge below holds focus for a whole chat
line.

### The touch driver

`crates/digblock-core/src/touch.rs` is pure logic with injected time, so every threshold is unit-testable. Touches
arrive normalized to 0..1 of the window.

- Left half: a virtual joystick anchored at touch-down and never re-anchored. Full deflection is a tenth of the screen
  height.
- Right half: relative drag-look, 300 degrees per screen width. A touch that stays inside the tap slop for 200ms
  promotes to digging at the crosshair; moving during that dig still looks, so the crosshair steers the target.
- Double tap on the look side jumps. Only clean taps chain, so flick-looking and mining never jump.
- Placing is deliberately not a touch verb, because a place tap's first touch is indistinguishable from a double-tap's
  first touch. It belongs to a HUD button.
- The pre-slop delta is dropped on the crossing frame; accumulating it would snap the camera by the whole slop.
- `tick(now)` is polled once per engine update, because a stationary finger sends no move events and nothing else can
  observe the hold threshold crossing.
- Touch look never inverts. Inverted Y flips the mouse delta only; touch look is dragging the world.

The touch HUD in `crates/digblock-client/src/ui/hud.rs` shares one set of pure layout functions between drawing and hit
testing, deliberately not the render pass's hit rectangles, which are one frame stale exactly when a screen has just
opened or closed under the finger. Thumb buttons are 64px against the corner button's 44px because they are held and
hammered mid-motion rather than visited. While any UI surface is open, the primary touch synthesizes cursor motion and a
left click at one site, so no screen needs to know about touch, and on release the cursor moves to (-1, -1) because a
lifted finger is nowhere and every hover would otherwise stay pinned under the last tap.

### Safe-area insets

Script cannot read `env()` directly, so `apps/web/src/lib.rs` creates a hidden fixed element whose padding is the four
`env(safe-area-inset-*)` values, reads the computed style, and multiplies by `devicePixelRatio` because the HUD
coordinate space is physical pixels. Known gap: the hotbar row itself uses a plain bottom margin and does not consult
the insets; only the corner, place, jump, pick, and spell rectangles do.

### The soft keyboard

`crates/digblock-platform/src/webtext.rs` appends a hidden `<input>` to the body: 1px, opacity 0, parked at the top-left
corner because iOS scrolls the page to it when the keyboard rises. The rules it encodes:

- The DOM input is the authoritative buffer while a field is active. Every `input` event restates the whole value, which
  the engine takes through `text_replace`. Forwarding per-key events is a trap: Android IMEs compose plain typing,
  delivering neither keydowns nor `insertText`.
- `beforeinput` catches `insertLineBreak`, because some IMEs deliver Enter as a line-break edit instead of a keydown.
  The value is read on `input`, not `beforeinput`, so composition updates are seen after the IME committed them.
- Focus must be taken inside the activating tap's own dispatch; a mobile browser only honours it there. winit re-focuses
  the canvas on every pointerdown, so the bridge re-takes focus whenever the field is active and the DOM disagrees.
- The falling edge blurs the input and re-focuses the canvas. A blur alone leaves focus on the body, where winit
  delivers neither motion nor keys, and a canvas is not editable, so the keyboard drops.
- Focus ownership is keyed on game-side state, not the DOM flag, because the canvas's blur is dispatched before the
  input's focus. Treating that blur as a real focus loss closed the chat on the very keypress that opened it.
- The bridge is constructed unconditionally. Gating it on device detection is how Brave's fingerprinting shields once
  left a phone with no keyboard at all.

### Audio unlock

Chrome refuses audio before a user gesture. `apps/web/src/audio.rs` attaches `pointerdown` and `keydown` listeners that
call `AudioContext.resume()`; they persist rather than firing once, because resuming a running context is a no-op. A
`visibilitychange` listener also resumes on return to visible, because iOS suspends the context when the tab backgrounds
and does not reliably bring it back; a resume there needs no gesture, since the context was gesture-unlocked already.
Commands that arrive before a sound finishes decoding are queued if they are loops and dropped if they are one-shots,
because replaying a click seconds late is worse than skipping it. Any failure degrades to a null sink.

## 5. WebGPU in the browser

The renderer is wgpu 30 on every platform. The web-specific knowledge is a short list of things that are silently
different in Chrome.

### Dropping a buffer frees nothing

`crates/digblock-client/src/gpu_ledger.rs` states the most transferable lesson in the repo: on the WebGPU backend,
`wgpu::Buffer` and `wgpu::Texture` have a no-op `Drop`. The GPU allocation lives until V8 collects the JS wrapper, which
it does late and in bursts, because the wrapper is a few bytes and V8 cannot see the megabytes behind it. A session that
walks (chunk buffers past the pool ceiling), resizes or toggles fullscreen (a whole target set), or moves the
draw-distance slider (a 200 MiB voxel window) strands all of it in Chrome's GPU process.

The rule is `destroy()`, never drop. It is safe with frames in flight on both backends: WebGPU orders it on the device
timeline after every earlier submit, and wgpu-core queues the raw handle behind pending submissions. The one constraint
is that nothing submitted later may still bind the object, so each retirement site replaces the bind groups naming it in
the same breath. The ledger tracks live bytes by kind and a `retired` counter that must stay flat while the player
stands still; the overlay's `mem` line shows both.

### `create_buffer_init` is unsafe on wasm

The util helper copies through a JS `Uint8Array` view of wasm linear memory with wgpu-internal allocations in between.
If one of them grows wasm memory, every live view detaches and the copy throws. `Queue::write_buffer` is immune: its FFI
glue builds the view and hands it to the synchronous GPU copy in one step. The renderer's `buffer_init` wraps
`create_buffer` plus `write_buffer` and is used everywhere for uniformity. Sizes must be u32-aligned.

### Never call `Device::push_error_scope` from the browser

wgpu reads a popped scope through a `JsOption` that counts only `undefined` as absent, while a clean `popErrorScope`
resolves `null`, which then reaches a converter that panics on anything but a validation or out-of-memory error. Every
pop aborts the module, error or not. `crates/digblock-client/src/pipeline_builder.rs` works around it with hand-written
`#[wasm_bindgen] extern "C"` bindings for `pushErrorScope` and `popErrorScope` against the raw `GPUDevice` from
`device.as_webgpu()`.

The reason error scopes are needed at all: an invalid pipeline poisons the whole encoder that binds it, and the frame's
one `finish()` drops every pass, so the symptom is a black window with no HUD. Draws the frame can live without (the
block outline, the dig crack) are built under a guarded scope, and their pipeline accessor returns `None` until the
scope clears.

### Buffer size limits and the black screen

The one limit raised above the WebGPU default is `max_buffer_size`, set to the adapter's own ceiling. Chrome zero-fills
a fresh 3D texture through a staging buffer the size of the whole texture, and the GI voxel window passes 256 MiB at
draw distance 24. A texture past the limit is never cleared: every frame errors and the screen goes black. The engine
caps draw distance to whatever the device granted (`gi::max_draw_dist`).

### Depth copies

WebGPU forbids `copy_texture_to_texture` into a `depth32float` destination. The depth "copy" between the scene and water
passes is a fullscreen draw into an `R32Float` target (`depth_blit.wgsl`), which is why the water shader loads depth as
a colour and gets a `vec4`.

### Surface configuration

- The surface format is the first non-sRGB format the adapter offers, for gamma-space rendering all the way to the
  surface. `Bgra8Unorm` is universal on Metal.
- `COPY_SRC` is omitted from the surface usage on wasm, because browser canvas textures do not reliably support it. Web
  screenshots snapshot the canvas through the browser instead.
- Adapter request uses `apply_limit_buckets: false` to take real limits rather than a portability tier. Init is async
  because the browser main thread cannot block; native wraps it in `pollster::block_on`. It is fallible because "no
  adapter" is an ordinary runtime condition on the web that must reach the page's error UI.

### Timestamp queries are off by default on web

Chrome's Metal backend samples timestamps at every pass boundary, which hands the GPU the frame as a dozen small pieces
with a drain between each, and macOS picks the GPU's performance state from whether a command buffer looks saturating.
Measured on an M2 Max: with queries on, a 120 fps scene fell within a minute to about 50 fps at 533 MHz. The native
build, whose timestamps let passes overlap, is unaffected. `?gputimer=1` opts in.

### Allocation patterns that matter on Chrome

Every buffer create and destroy is a GPU-process allocation plus a deferred free that wgpu batches into periodic cleanup
spikes. The chunk renderer therefore:

- Uses one shared index buffer for all chunk meshes, allocated once.
- Allocates vertex buffers in 16 KiB size classes, so a remesh nearly always lands in the same class and is written over
  the live allocation.
- Keeps a free-list pool capped at 16 MiB; past the ceiling, buffers are destroyed, not dropped.
- Grows the per-chunk origin instance stream at 85% occupancy, so the reallocate-and-repopulate lands in one frame
  rather than at overflow.
- Orders draws front to back in 48 coarse distance buckets. Immediate-mode GPUs, which Chrome usually runs on, get their
  early-Z rejection from draw order alone.

Upload budgeting is by bytes as well as by time, because `write_buffer` merely records the copy and Chrome's GPU process
performs the staging transfer at submit, after everything the engine can measure. One shared deadline covers jobs,
result draining, evictions, and uploads together; separate slices summed past the budget.

### Fonts

Both embedded TTFs are Latin-subset, because fontdue outlines every glyph reachable from the cmap when a face is loaded.
An unsubset face costs tens of milliseconds and megabytes of heap at startup, on the wasm main thread.

### Shaders: three gates, and what Tint rejects

WGSL has no include mechanism, so `crates/digblock-assets/src/lib.rs` prepends shared preludes with `concat!` in a fixed
order, each prelude the sole definition of one thing so passes cannot silently disagree. Preludes are also validated
standalone, because one that only ever compiles concatenated can pick up a silent dependency on whatever precedes it.

Validation is three non-overlapping gates:

1. **naga parse and validate**, in a plain `cargo test`. The permissive end.
2. **Full wgpu-core validation with `Backends::NOOP`** (`every_pipeline_builds_without_a_gpu` in the renderer). This
   catches what naga cannot: bindings against the hand-written layouts, vertex formats against declared attributes,
   fragment outputs against target formats. A sibling test uses `naga::proc::Layouter` to assert the shader-side size of
   the frame uniform block equals `size_of::<FrameUniforms>()`.
3. **Real Chrome's Tint**, via `tools/shadercheck` (`just check-shaders`). It writes a harness page, runs headless
   Chrome with a dedicated profile, calls `createShaderModule` and `getCompilationInfo()` per shader, and reads stderr
   rather than awaiting exit, because a headless run that finished does not necessarily exit. Dawn publishes no
   standalone `tint` binary, and a pinned one could drift from the Chrome a player runs.

Tint rejects WGSL that naga accepts, and the failure is a whole invalidated pipeline in the browser only, with every
native gate green. That has shipped twice. The known cases, each documented at the shader that had to be rewritten:

- **Unparenthesized mixing of `*` and `^`.** `a * b ^ c * d` needs parentheses. This cost the block outline and the dig
  crack on web (`highlight_crack.wgsl`).
- **Dynamic component indexing of a vector.** Use `select` instead of `v[i]` with a computed `i` (`siphon.wgsl`).
- **Derivatives in non-uniform control flow.** Take `dpdx`/`dpdy` before any early-out or branch; the value may be used
  inside a branch, only the call site must be uniform (`gi_resolve.wgsl`).
- **Dynamically indexed function-scope arrays** are the construct a mobile driver is likeliest to refuse, which is why
  the outline and the crack are separate shader modules and the crack is built guarded.

The workspace pins `naga` at the same major as `wgpu`, because an older naga accepts WGSL the runtime rejects, which is
the single failure the first gate exists to catch.

## 6. Real-time global illumination

Probe-traced GI is the only lighting path. There is no shadow map, no light map, and no flag to turn it off. The whole
thing runs on a half-resolution fragment pass, two tiny compute dispatches, and one 3D texture of the world, and it runs
on a phone. This chapter is the set of techniques and the hacks that made each one affordable. Code lives in
`crates/digblock-client/src/gi.rs` and `gi/`, the shaders in `assets/shaders/gi_*.wgsl`, and the consuming half in
`chunk.wgsl`.

### The frame, in order

1. **Probe update** (compute), at the head of the encoder so every later pass sees this frame's atlas: flush the light
   list, copy the previous probe atlas forward, trace 512 probes per cascade, run one bookkeeping invocation for the
   camera's own light, and run the edit-invalidation sweep if an edit is live.
2. **Depth prepass**, writing depth and a rasterized face-normal code into an `R8Uint` target cleared to a sentinel no
   real face carries.
3. **Depth blit** to `R32Float`, because WebGPU will not copy into a depth texture.
4. **GI resolve** at half resolution (or a third, or a quarter), two `Rgba32Float` targets: indirect light with view
   distance in alpha, and sun visibility with the face code, water path length, and penumbra width packed beside it.
5. **À-trous** twice, ping-ponging, with the spare target parked as next frame's history.
6. **Scene pass** bilaterally upsampling the pair, and every later pass (water, players, viewmodel, spray) gathering
   from the same bind group.

The GI divisor is an integer, and that is a requirement rather than a convenience: the upsample is a four-tap gather
whose 2x2 base would drift off its texels at any other ratio.

### The voxel window

The world the rays see is one `Rg8Uint` 3D texture, one texel per block, the full 256-block world height tall and twice
the draw distance on a side. The red channel is a material look (a row in a small LUT: albedo, cast light, opacity), the
green channel is a cover index for shaped cells (stairs, slabs, doors) into a table of 12x12x12 bitmasks. Look and cover
are split so a slab in a new material costs neither a new look nor a new cover.

- **Toroidal in XZ, never in Y.** Chunks upload at `pos.rem_euclid(side)`, so scrolling is a write of the chunks that
  changed and nothing moves. Y is the full world, so an upward ray always reaches sky and the window never scrolls
  vertically. The wrap is one modulo per axis with a select, not the textbook two, because on a GPU each `%` is an
  emulated sequence and the marches were standing a stack of them between every DDA step and its texture load.
- **Twice the draw distance.** One draw distance on a side drew a lit, shadowless, darker ring around any camera high
  enough to see past the horizon haze: past the half extent the sun ray answered "lit" on no data and the gather fell
  through to the sky dome. Every march breaks at the lateral bound rather than reading a texel the wrap aliases onto
  another part of the world, and the CPU light-list filter uses the same bound, pinned by a test.
- **Chrome zero-fills a fresh 3D texture through a staging buffer the size of the whole texture.** At draw distance 24
  the window is 288 MiB, past the WebGPU default buffer limit, the clear fails every frame, and the screen goes black.
  The engine requests the adapter's real limit and caps draw distance to what was granted.

**Coarse occupancy.** A second `R32Uint` texture holds one word per 8x8x8 brick: any-blocker and any-emitter bits, two
density bytes, the modal look, and the lowest and highest column tops inside the brick. It has two uses:

- **Empty-brick skip.** A zero word is one jump. The skip advances the DDA arithmetically rather than reconstructing
  `origin + dir * t`, which lands past the exit plane and misses a cell at a brick edge: periodic, not rare, with the
  sun pinned to `dir.z == 0`. It lands on the last cell inside the empty region and lets the ordinary step cross out, so
  every iteration advances at least one cell and the march cannot stall on its iteration cap.
- **Brick as medium.** Past a per-march distance, a ray does not enter a brick at all; it survives with a probability
  from the brick's density and an LCG rolls it. The density is layered (open above the highest top, core below the
  lowest, a band between) because one density over the whole brick darkened every horizon: a ray skimming flat ground
  attenuated inside the ground's own surface brick.

One wrap serves both levels, because the side is a multiple of the brick.

**Sky ceiling.** A CPU mirror of the brick rows tracks the top cell of the highest brick holding anything. A
non-descending ray above it is a proven miss in one float compare. That is most of a screen full of water's sun cost;
proving open sky one brick at a time was a string of dependent texture loads per fragment. The mirror needs owner
tracking on eviction, because toroidally an evicted chunk's slot may already hold its replacement, and without it the
ceiling ratcheted upward over a session.

**Edit invalidation.** Edits queue into a CPU ring capped at 256, dropping the oldest, because the newest edits are the
ones whose light is most wrong. Up to four drain per frame into uniform sphere slots; the invalidation kernel resets
only the age byte of probes inside the sphere. A carried torch is not a second mechanism: it fires the same invalidation
every two blocks of travel, triggered by distance and never by a cell crossing, which on a boundary fires every frame
for a player standing still.

### Probe cascades

Two toroidal cascades of ambient cubes: a fine one at 2-block spacing covering 64x48x64 blocks, and a coarse one at
8-block spacing covering 256x160x256. Each probe stores six axis colours in an `Rgba16Float` 3D atlas laid out as six Z
slabs, with alpha as the alive bit. Gathering is `n * n` over the three axes a normal faces.

- **Toroidal addressing with a self-heal checksum.** A probe texel serves whichever world cell is congruent to it around
  the camera. A flags texture packs a checksum of that cell in the high 16 bits and an age in the low byte; a mismatch
  means the window scrolled, and the probe restarts at full blend. The checksum is checked at read time too, not only
  when the sweep reaches the probe, because a full sweep is 48 frames. A fragment reading a wrapped probe is wrong for
  one pixel for one frame. A probe reading one folds the wrong radiance into its own stored value and hands it on as
  bounce, every sweep.
- **Age drives the blend.** Alpha is 1.0 at age 0, then 0.5, 0.25, and 0.1 at steady state. The sun direction is
  quantized, and a quantization step deliberately does not reset ages, so shadows glide across a step on hysteresis.
- **Stateless relocation.** A probe whose lattice cell is solid nudges up to one block into the first air neighbour in a
  fixed order: up first (cavities open upward), then the four sides, down last. The resolve recomputes the identical
  offset from the same voxel window, so no flag bits carry it.
- **Dead probes vote nothing.** A probe still solid after six tries stores black with alpha 0, and three sites honour
  that separately. The cascade gather drops it out of the weight sum rather than folding in its zero, so a pixel
  surrounded by dead probes falls through to the coarser cascade and then the sky dome instead of averaging toward
  black; blending them in is what painted the dark patches. The bounce read returns no bounce, because a live probe
  sampling a buried neighbour's zero and storing the dimmed result is a feedback loop. The camera-light kernel skips
  them and, if all eight neighbours are dead, answers the depth-faded sky, which is what stopped waterfall spray from
  going fully black from certain standing spots.
- **Permuted sweep.** 512 probes per cascade per frame, chosen by `k * stride mod count` with a stride near the golden
  ratio nudged coprime, so a cycle visits every probe once and consecutive dispatches land far apart. A contiguous
  cursor swept the volume as a visible band. Tests pin that the permutation is complete, that consecutive steps are far
  apart, and that the multiply cannot overflow in the shader.
- **The forward copy is cheap and its measurement is a trap.** The previous atlas is copied whole each frame so
  untouched probes carry forward. It costs 9 microseconds. Measuring it by deleting it is invalid, because the atlas
  then holds garbage and the downstream workload changes; measuring by adding four more copies is exact because the
  result stays bit-identical.

### The probe trace

One workgroup per probe, 32 rays in a spherical Fibonacci spiral rotated per update by the frame counter, folded
cosine-weighted into the six axes.

- **Infinite bounce from last frame's atlas.** A hit returns albedo times (the previous atlas at the hit point, plus
  next-event light, plus sun at the hit). One ray therefore buys the whole multi-bounce series.
- **Shadow rays never cross a brick by summary**, because an occluder is a cell, not a density. Gather rays do, from two
  bricks out for the fine cascade and one for the coarse, on the footprint argument: a probe's ray is the lowest
  frequency sample in the engine.
- **Emissive hits return zero.** This term is radiance, so it does not fall off, and a probe that catches a torch on one
  ray folds a huge value into a six-axis average that then blends for a whole sweep. The cell still occludes; emission
  arrives only through next-event estimation.
- **A miss that merely ran out is not sky.** A proven escape (past the ceiling) keeps full daylight at any depth. An
  exhausted ray is multiplied by a sky fade at the exhaust point, so a deep tunnel cannot be lit by leaking daylight.
  The sky as a GI source is desaturated a third of the way toward its luminance, because the zenith blue is tuned for
  looking at, and bouncing it at full saturation tints every shadow cold.
- **Decorrelate everything.** A ray's random draw is hashed from ray index and frame; the point chosen on an area light
  comes from hashes of that draw, because a fixed point on every rectangle bakes the face's centre line into the cache;
  the brick roll seed mixes probe, light, and frame.

One extra single-invocation kernel computes the light at the camera itself, trilinear over the eight surrounding probes
plus one sun march, for things with no world position: spray and the viewmodel. Its sun visibility is rgb, not a grey
lane, because sunlight reaching a submerged camera is red-starved.

### The per-pixel resolve

Per half-resolution texel: one cosine-hemisphere gather ray up to 32 blocks, terminated in the nearest probe with no
relocation walk and no occlusion test; one shadow ray to a light chosen by resampled importance sampling; and one sun
ray along the exact, unquantized direction. Cosine sampling is chosen so the cosine and the inverse pdf cancel and the
gather is the returned radiance itself, with no factor to get wrong.

- **The face normal is rasterized, never derived.** Reconstructing it from screen derivatives at half resolution is
  garbage on every silhouette, where it poisoned the ray origin, the ambient-cube weights, and the backface term at
  once. The prepass writes a code per face, and the same code table serves the upsample's equality test.
- **Derivatives are hoisted above every early-out**, because Tint enforces uniform control flow for them and naga does
  not. Before that fix, the browser shipped with an invalid resolve pipeline and no GI at all.
- **The gather reaches the full lateral bound and nothing narrower.** A 32-block safety margin meant a ring of pixels
  took the cascade blend, which past the coarse cascade is the sky dome, and distant grass came out visibly blue.
- **Footprint-driven coarse crossing.** A half-res texel already spanning a brick cannot resolve a bounce at cell scale,
  so its gather crosses by summary from the first brick. The footprint is clamped because the derivative is garbage on a
  silhouette.
- **A hit's own sun visibility is read back from last frame's sun target** where the hit is on screen, marched only
  where it is not.

**The sun ray is deterministic per world point, and the penumbra is reconstructed spatially.** The cone jitter is hashed
from an eighth-block world cell pushed slightly into the surface, never from the screen position, because a moving
camera re-asking a different question per world point makes penumbra bands shimmer. The cone is half a degree,
deliberately wider than the real disc: it buys a penumbra sharp at contact that softens with occluder distance at no
extra ray. The occluder distance times the cone tangent over the texel's world size is written out as a penumbra width
in texels, and the denoiser blurs to exactly that. Two tests pin the tangent to the cone and the maximum width to the
filter's reachable support.

**Temporal accumulation** is a running average in `Rgba32Float`. That format is load-bearing: both targets hold an
average, an 8-bit target rounds a 1/N difference to zero, and 16-bit float failed the same way one format up, with every
pixel freezing where it stood when the camera stopped. Two tests pin the mantissa against the maximum count and the
packed code-plus-count against exact integer round-tripping.

- **Two-state cap, not a rate.** 512 samples at rest, 48 while moving, 12 for the sun always, 8 to prime a disocclusion.
  A per-frame rate was built and rejected: off-lattice, the blur compounds at the rate the sub-texel phase sets rather
  than the speed, and a pixel creeping a hundredth of a texel a frame still sweeps the whole phase range across a long
  history. There is no useful middle.
- **Smear is bounded by construction.** A history can only be dragged as far as its own length, which is why the sun
  lane, being deterministic, keeps only 12 frames.
- **Reprojection is exact for static geometry** with no motion vectors: reproject the world position through last
  frame's view-projection. Each bilinear tap is validated on exact face-code equality and on the distance the point
  would have been at from the old camera, because comparing stored view distances directly rejects the whole screen the
  moment the camera moves. Rejected taps stop voting and the survivors renormalize. A nearest tap smears along the
  direction of travel.
- **Stillness needs two guards.** Below 0.15 texels of travel the fetch snaps to the pixel's own lattice position,
  because the round trip through the two matrices drifts a tenth of a texel on a dead-still camera and resampling that
  at 450 samples compounds into streaks. An exact CPU matrix compare rides in the uniform as well, because the shader's
  float round trip fails a thousand blocks from the origin and was capping resting pixels at the moving count.
- **Grazing surfaces get a second, relaxed validation pass.** A surface seen edge-on spans more depth per texel than the
  flat tolerance allows and churned as a flicker band; the relaxed tolerance scales with the texel's world size. A tap
  that misses by a small margin is grazed, not disoccluded, and restarts unbiased.
- **Disocclusion primes from the probe cache.** A reveal starts at count 8 from the cheap cache lookup plus one light,
  so it sharpens instead of developing out of speckle. Two gates, each earned by a measured regression: only where the
  sun is weak (on sun-lit ground the prime's bias measured an order of magnitude worse), and only on a real reveal,
  never a graze (ungated, walking backward locked the near field at cache lighting).
- **History resets on a sun jump.** A cosine below 0.995 between this frame's sun and last frame's means time was
  teleported, and screen history that survives it ghosts for seconds.

**Adaptive ray budget.** Each 8x8 tile rolls a refresh period from its view distance: far pixels trace less often, up to
every fourth frame for indirect and every sixteenth for the sun. The roll is per tile so a wave either works or idles;
per pixel, the lanes that skipped would wait on the ones that did not and save nothing. Never skipped: pixels with no
history, inside an edit sphere, with a young count, moving pixels beyond a period of two, and any sun edge texel. Edges
must trace because the bounded-smear argument only holds on a locally constant field, and because the denoiser dilates
the penumbra lane, so a skipped trace must never re-feed a dilated width. Iteration budgets are budgets, not capacities:
the shader loop bounds are the ceiling, and a budget moves only on a measured call, because it can only truncate, which
silently un-shadows whatever lies past the cut. That bug shipped once, un-shadowing three percent of pixels.

**Chromatic sun through glass.** The sun lane is scalar; the glass filter is rgb. The sun lane carries visibility times
the filter's luminance, and the indirect lane carries exactly the difference, multiplied by the twilight strength,
because the residual is negative in absorbed channels and would otherwise subtract real light out of the twilight.

### The à-trous denoiser

Two iterations, strides 1 and 2, a three-tap kernel, no moment buffer. Doubling the stride instead of growing the kernel
is the whole trick. A third iteration is deliberately absent; on a half-res buffer its reach is most of a shadow.

- **Analytic variance instead of a moment buffer.** Indirect variance is luminance over the square root of the count;
  sun variance is a Bernoulli's `p(1-p)/n`. That saves a ping-ponged 32-bit float target and cannot drift out of step
  with the accumulator.
- **Edge stopping** is a hard reject on face code, a depth tolerance that scales with stride and view distance, and each
  signal stopping on itself at four sigmas. Without the last one the kernel blurs straight across a lighting boundary on
  a flat floor, where depth and normal are identical and only the lighting differs.
- **The sun filter is reconstruction, not denoising.** It runs at rest too, and inside a penumbra it deliberately does
  not edge-stop on the sun value, because a width of zero would collapse the kernel to identity and contact would stay
  hard. The penumbra width is dilated by max over same-surface taps and written back, so the lit half of an edge learns
  it borders a shadow and blurs by the second iteration.
- **Metadata rides through untouched.** A filtered face code is not a code.
- **One uniform buffer per iteration.** Both passes sit in one submission, so a single rewritten buffer hands both
  whichever stride was written last.
- **Two timer brackets.** Sharing one query pair lets the second iteration overwrite the first.

### The upsample and the shading

`chunk.wgsl` takes four GI taps around each full-resolution pixel, weighted by view-distance proximity and hard-gated on
face-code equality. If no tap matches, it takes the closest tap whole rather than averaging four wrong answers; the old
code gave them all the same negligible weight, which normalized away to exactly that average.

- **Twilight multiplies the radiance, never the shadow.** An earlier `mix(1.0, vis, strength)` returned 1.0 at twilight,
  discarding the traced visibility, and every surface facing the caster took full unshadowed light through walls. A
  sealed vault lit one wall four times the other, and which wall flipped at the horizon crossing.
- **Per-block tint multiplies albedo, never light.** The denoiser and accumulator only see the lighting buffers, so a
  step written into the light is exactly what they smooth away, and the probes' albedo stays untinted so a block's
  jitter never rides its own bounce.
- **Glossy finishes reflect the probe cache.** Metal and polished stone read the probe cache along the mirrored view
  ray. Probes hold cosine-weighted irradiance, so the reflection is pre-blurred to about the width polished stone wants,
  with no light scan and no shadow march per fragment. Lamp highlights are unshadowed Blinn over the first eight entries
  of the light list, which is why the list is sorted nearest the eye.
- **Vertex positions are `@invariant`** on both the prepass and the scene pass. Once the prepass grew a fragment stage
  the two pipelines stopped optimizing position math identically, and one ulp fails the depth test, leaving solids full
  of view-dependent holes.

### The light list

Up to 256 lights on the GPU, with a runtime cap that defaults to 128 because the list was priced at about 8.6
microseconds per flushed light per frame. Selection on the CPU (`gi/lights.rs`) is a sequence of culls followed by a
ranking:

1. Skip chunks whole that lie beyond the march bound, because a vein contributes a rectangle per exposed face and caves
   are made of exposed faces.
2. Drop lights behind the camera beyond one gather march plus the light's own reach.
3. Drop lights below a sky floor once the camera is well above the surface. The floor is a low percentile of sampled
   column tops minus a cushion, because at open reveal heights 85% of a player's seam lights sit below the local
   surface. The percentile keeps a lone shaft from disarming the cull; the cushion keeps a lit bed under an ocean lit.
4. Rank by distance to the segment from the eye to 24 blocks ahead, not to a point, so the carrier's own light can never
   be the one dropped while a candidate ahead still outranks its mirror behind. `select_nth_unstable_by` is O(n), which
   matters because a carried light moves every frame.
5. Sort the survivors nearest the eye for the glossy read.

Re-flush is exact-equality based so a standing player pays nothing: a moved anchor with a changed reachable count, a
carried light that moved at all (an epsilon would decide when a slow creep stops lighting a room), a changed cap or
floor or window.

**Area emitters.** A revealed ore vein's exposed faces are merged into rectangles by the mesher, because the chunk store
holds stone where a vein is and the classifier cannot see one. A rectangle is weighted by its cosine and area; a point
source at a seam's middle read as a hotspot and spilled light around the corner of the rock it is set into.

**Shadow-ray margins** come from one function so the three consumers cannot disagree. A point light's margin is the
farthest corner of its own cell plus slack, derived rather than a half-diagonal because a shaped emitter's origin is its
flame, not its centre. A rectangle's margin is slack only, since its sample point lies on the face. A carried light's
margin is zero: its origin is on the carrier's capsule axis and provably in air, and a wall right beside it should block
it. Without that, a carrier standing on a block top sits at the degenerate maximum and the margin swallowed a whole
wall.

**Three estimators, one price.** The resolve uses resampled importance sampling with one LCG draw per accepted
candidate, which is load-bearing: reusing one draw makes the tests nested events on one variable. Players, held items,
and confetti take the brightest four candidates with a ray each, because one binary visibility sample on the whole pool
is right in front of a temporal filter and wrong in front of nothing; handing players the reservoir produced hard-edged
black squares crawling over bodies. Probes pick one light stochastically. The pricing constants are pinned identical
across the two shaders by a test that compares named constants, because a bare literal has no name to compare, and that
is how a stale gain survived a retune once.

**Calibration.** A near radius of two blocks in the inverse-square law is a display-range fix: a torch half a block from
its support block lit it at eight times white, and three percent of pixels had a channel at 255 with none of them
actually white. The gain is a ceiling, not a preference; past it the clip fraction turns over.

**Torch flicker** is a two-harmonic sine keyed on the frame clock, with a spatial phase seed for placed torches and a
zero seed for a carried one so its phase is stable while its owner moves. A carried light's origin is a function of
position only, never facing, so mouse-look never moves the light and turning in place is not a moving emitter.

### Media and sky

One participating medium serves the whole engine, defined in a dependency-free prelude so the compute kernels can share
it. The in-scatter term is the same sky function the sky pass draws, so a surface fades at distance to exactly the sky
pixel behind it, sunset glow included, and the draw-distance rim disappears. Fogging toward one flat colour is the wrong
quantity, not a curve to retune. Air extinction is Koschmieder's constant over a visibility distance, so the knob means
"how far can you see". Water extinction is a measured per-channel sigma, and it is the only water extinction: the sun
march returns a water path length, not a transmittance, so view and sun rays cannot disagree.

Every answer that says "sky" without proving a path to it is multiplied by a depth fade pinned to sea level: the cascade
fallback past coverage, an exhausted ray, the sun march's exits, the camera light's all-buried fallback. A proven escape
keeps full daylight at any depth. That single distinction closed the report of long tunnels lit by leaking daylight,
while leaving the surface bit-identical.

### Where the time goes

From the recorded flyover bench at 120 fps, per frame:

| Pass                     | Milliseconds |
| ------------------------ | ------------ |
| Probe compute            | 0.24 to 0.27 |
| Resolve                  | 2.7 to 4.3   |
| Denoise, both iterations | 5.8 to 9.0   |
| Scene                    | 3.4 to 5.0   |

The stub attribution says the same thing more sharply: removing the final gather saves half the frame, removing the
320-iteration sun ray saves a quarter, and emptying the chunk raster saves nothing on a tile-based GPU. Stub bits ride
in a uniform so every branch is wave-uniform and an un-stubbed run pays nothing.

Rendering at native resolution with the GI divisor at 3 costs the same as rendering at 70% scale with the divisor at 2,
and the image differs by a mean of 0.11 out of 255 against 18.6. The denoiser and upsample already reconstruct that
signal. So the high-DPI default keeps native resolution and drops the GI divisor.

The worst case is scripted: stand on glass at the top of the world, stare straight down, do not move. Every half-res
texel is far terrain seen through glass, so all three marches run at their longest, the glass pass gathers on top, and
nothing reprojects away. Three stations over a kilometre apart, so each goes through the arrival path as a teleport does
and never inherits the previous probe field.

### Built, measured, and rejected

- **A ReSTIR temporal reservoir** in front of the accumulator. Validated unbiased, cut the raw term's flicker four
  times, and the finished frame did not move, because the accumulator and denoiser already average over a longer window
  than any clamp. Reverted.
- **Seeding a remapped probe from a converged neighbour.** Zero effect at every settle length. An age-0 probe is
  trilinearly blended, bilaterally upsampled and temporally accumulated, so it barely reaches the screen.
- **Sweeping every probe in one frame.** Bought a third of the transient at most. The probe field carries the transient
  and it is inherent to a world-space cache at this cadence. The work moved to the per-pixel gather instead.
- **A third, coarser skip level.** Measured out in both orderings; at terrain altitudes its bricks almost always hold
  something.
- **A brick-top exact skip.** The per-cell compare cost more than the crawl it saved, and water raises no top.
- **The B3 spline denoiser kernel.** A slightly smaller residual for twice the taps, at a point where the artifact is
  already gone.
- **A history-gradient clamp.** An exact no-op, because it read the gradient from the already smeared history.
- **A storage-buffer rewrite** to delete the forward atlas copy. The copy costs a tenth of a percent; the rewrite was a
  real WebGPU portability risk.
- **Blue noise.** Behind one function, unpaid for until hashed noise is shown to be insufficient, because it costs an
  embedded asset.

### Gotchas worth stealing

1. A toroidal atlas always returns a texel. Every read outside coverage returns another place's light, and three
   separate guards were needed: for fragments, for the probe kernel's own bounce, and for the previous-frame read.
2. Step first, then test. A march that tests the origin cell first is harmless for a cube and fatal for a shaped block
   whose surface lies inside its cell; every slab, stair, and door top went black.
3. One surface epsilon, applied inside each march and nowhere else. Pushing off at the call site too stacked into more
   than a whole cell of geometry that could not shadow anything.
4. Emission must not reach the gather, and an emitting shaped cell must not be a whole-cell hit; that rule was casting a
   full cube's shadow from a thin post into every other torch's shadow ray.
5. Bench at the real window size. At the default bench window the frame is not GPU-bound and every stub measures slower
   than baseline. Pin the capture to one monitor, or a frame-time bracket compares a 120 Hz screen against a 60 Hz one
   and reads the GPU downclock as the change.

## 7. Multithreading on the web

There are no wasm threads. The chunk pipeline in `crates/digblock-core/src/pipeline.rs` runs workers as pure functions
over immutable snapshots, and the executor is a runtime enum with three arms: inline, native OS threads over crossbeam
channels, or an injected `JobDispatch`. The web build injects a Web Worker pool through that trait.

```rust
pub trait JobDispatch {
	fn ready(&self) -> bool;
	fn submit(&mut self, job: Job) -> Result<(), Job>;   // non-blocking; hands the job back when full
	fn poll(&mut self) -> Option<JobResult>;
	fn lost(&mut self) -> Option<JobKey>;                // jobs the pool can no longer answer for
	fn worker_count(&self) -> usize;                     // the truth, not what was requested
	fn reset(&mut self);
}
```

Everything is non-blocking because it is called from inside the frame. `runs_inline()` reads `ready()`, not submit
refusals: a refusal means busy, not unavailable. That distinction shipped as a bug once, when every slot wedged, submit
refused everything, `ready()` stayed true, and nothing streamed and nothing fell back for the rest of the session.

### A second wasm module

`apps/web-worker` is a 34-line crate exposing one function, `run_job_bytes(&[u8]) -> Vec<u8>`, with `digblock-core`,
`wasm-bindgen` and the panic hook as its only dependencies. It is a separate module for two reasons:

- The main module's `#[wasm_bindgen(start)]` calls `web_sys::window()`, which does not exist in a worker; instantiating
  it there would panic before the first job. The worker module has no `start` at all.
- Every worker instantiates its own copy with its own linear memory. Core-only keeps that small, and the low-RAM devices
  are exactly the ones the pool exists to help.

### Spawning and the bootstrap

There is no third `.js` file in the bundle. `apps/web/src/jobs.rs` generates the bootstrap in Rust and hands it to a
`Blob`, then `new Worker(blobUrl, { type: "module" })`:

```js
import init, { run_job_bytes } from "<absolute stamped url>";
const boot = init().then(() => self.postMessage({ ready: true }));
self.onmessage = async (e) => {
  const id = e.data && e.data.id;
  try {
    await boot;
    const out = run_job_bytes(new Uint8Array(e.data.job));
    self.postMessage({ id, out }, [out.buffer]);
  } catch (err) {
    if (id !== undefined) self.postMessage({ id, out: new Uint8Array(0), error: String(err) });
  }
};
```

Rules encoded in those ten lines:

- A blob rather than a shipped file, because a third unstamped `.js` would be a cacheable file whose contents can drift
  from the module it imports, which is the `LinkError` shape again. A blob URL is no base for a relative import, so the
  module URL is made absolute against `location.href` first.
- `await boot` on every message, not just the first, because messages can be posted before instantiation finishes.
- Every path answers. The handler is async, so a wasm trap rejects its promise rather than escaping as an uncaught
  exception, and only an uncaught exception reaches the parent's `Worker.onerror`. Without the catch, the reply never
  arrives and the chunk is stranded with no diagnostic.

### Message passing, not shared memory

Jobs cross as bytes in transferable `ArrayBuffer`s, encoded by a hand-rolled little-endian codec in
`crates/digblock-core/src/job_wire.rs`. A mesh job is about 11 KiB; a generated column result is up to 128 KiB. Every
read is bounds-checked, so a truncated or corrupt message fails the decode and never panics the client, and a bad length
is rejected before allocating. The air-chunk sentinel is part of the format: sky chunks share one `Arc` compared by
pointer, so encoding them as zeroes would decode into look-alikes and the mesher would start meshing the sky.

The copy each way costs less than the millisecond the job itself takes, so the transfer has never been the interesting
number. What the pool buys is the main thread.

### Pool policy

- `default_count()` is 0 when `navigator.hardwareConcurrency <= 2`, otherwise `min(2, cores - 2)`. Four workers was
  measured and declined: it fills 2.6x faster but is the only arm that drops a frame, because absorption is pinned to
  the thread owning the GPU device. `?workers=N` overrides for A/B, capped at 8.
- 16 jobs outstanding per worker. This is a feed rate, not a fairness knob: the pool refills once per frame, so
  throughput caps at `depth * workers * fps`. The pool shipped feed-rate starved once, with workers 9% utilized and a
  thousand columns queued.
- `ready` and `dead` are per slot, never per pool. One booted worker does not vouch for others that never will, and
  posting to an uninstantiated worker drops the job into a queue nothing services.
- Wedge detection counts sweeps, not seconds. A hidden tab throttles the workers as hard as the page, so a wall clock
  would declare a healthy pool dead on the first frame back from the background.
- Message keys are interned `JsValue`s, because `Reflect::set` with a fresh `&str` interns a JS string per call and
  submit runs up to `depth * workers` times a frame.
- Closures are kept alive in the slot; dropping one leaves the handler pointing at freed wasm.
- The shape editor refuses the pool, because a worker is a separate wasm instance with its own statics and would mesh
  the shipped shape instead of the live sculpt.

### The reset contract

A pool outlives the pipeline that fed it, because workers cost real time to start and hold no world state. A generated
result carries only a column coordinate, so one from the old world would land as authoritative terrain in the new.
`reset()` therefore guarantees that after it returns, no result for any earlier job is ever delivered. The
implementation is one monotonic counter: `min_valid_id = next_id`, and `poll()` drops any reply below it. It also
releases every slot's depth accounting, or the old world's jobs count against the limit on every slot and the new
world's first refill is refused everywhere while `ready()` stays true.

The pipeline side does the matching cleanup when it takes the pool back: rolls every submitted generation back and
re-queues every surviving chunk, because an abandoned mesh job was the only thing that would ever roll that counter
back, and a chunk whose counter never moves never meshes again, which reads as a permanent hole rather than a dropped
frame.

### Staleness by generation counter

Every mesh job carries the generation it was built from, and a result is accepted only if it matches the slot's current
counter. Generated results carry no counter and are gated on state instead: only a column still generating may take
terrain. Without that gate, a column evicted with its job in flight, re-entered and re-dispatched, produces two results
and the second overwrites live cells with pristine worldgen. Survivable in single player, fatal in multiplayer.

### Async without blocking

- The entry point is `#[wasm_bindgen(start)]` spawning `boot()` with `wasm_bindgen_futures::spawn_local`. The platform
  run function returns immediately on wasm; winit keeps the loop alive through the event system.
- GPU init runs in a spawned future and delivers its result into the winit loop as a user event.
- `yield_to_browser()` awaits a `requestAnimationFrame` and then a `setTimeout(0)`. The rAF alone is not enough: the
  await's continuation runs as a microtask inside the rendering task, before its paint, so heavy synchronous work
  scheduled there still starves the update the caller wanted shown.
- The frame loop is winit's redraw chain, not a hand-rolled rAF loop. The fps cap declines the redraw and asks for the
  next frame; natively it sleeps. Device-loss detection polls once on web instead of a sleep-and-retry loop.

### The streaming budget

`crates/digblock-core/src/stream_budget.rs` is an adaptive controller with a 3ms floor on web and a 10ms ceiling,
protecting a 16.7ms frame with a 2ms margin. It grows a quarter millisecond per clean frame over a 30-frame window and
collapses on a near miss. Samples of 100ms or more are discarded as not-frames, because background tabs throttle
`requestAnimationFrame` and a resume gap would otherwise collapse the budget. One subtlety: urgency peeks the work
heaps, which the worker refill drains into the pool, so the frame that hands the backlog over would read as idle and
collapse the deadline exactly as results start landing. The in-flight count is folded into urgency for that reason.

## 8. Networking

### Topology

Three parties, and every edge between them is a deliberate choice:

| Edge                  | Transport                             | What crosses it                                                               |
| --------------------- | ------------------------------------- | ----------------------------------------------------------------------------- |
| Client to master      | HTTPS, bearer tokens, any-origin CORS | Sign-in, session refresh, server list, join tokens, names, hiscores           |
| Client to game server | WebTransport over QUIC (UDP only)     | The 8-byte preamble, Hello, edits, snapshots, journal columns, chat           |
| Game server to master | HTTPS, server key as bearer           | Registration, heartbeat, token verification, skill reports                    |
| Master to game server | Raw QUIC handshake                    | The health probe: TLS 1.3, ALPN `h3`, certificate hash compared, nothing else |

The master (`apps/master`, .NET 10 minimal API, outside the cargo workspace) owns accounts, display names, the server
registry, and join tokens. The game server (`apps/server`) owns one authoritative fixed-tick world and never learns who
a player is except by asking the master. The client (`crates/digblock-net` behind either transport) never asserts its
own identity; it presents opaque bytes and the server asks the master what they mean.

There is no unregistered server mode and no tokenless join. A game server registers with the master on boot and retries
forever; the master probes it back over real QUIC before listing it; a client joins only by presenting a master-minted
token that the server verifies against the master with its own key. That closes the loop in both directions: a server
cannot list itself at an address it does not control, and a player cannot claim a name or an account the master did not
issue.

The master URL is one constant, `DEFAULT_MASTER_URL` in `crates/digblock-net/src/protocol.rs`, overridable by env var,
flag, or `?master=` on the web. An explicitly empty value disables master-backed features rather than falling back, so
hermetic capture runs never ping production.

### Why QUIC

The game needs an unreliable channel for inputs and snapshots and a reliable ordered one for edits and join sync. In a
browser the only API that offers both, with UDP underneath, is WebTransport, which is HTTP/3 over QUIC. The dedicated
server therefore terminates QUIC itself with `wtransport`, the native client uses the same crate, and the browser client
uses the browser's WebTransport. One protocol, three stacks, and no TCP anywhere: the deploy files publish the port
block as `/udp` only.

The cost of that choice is trust. A browser will not connect WebTransport to a self-signed server unless the page
supplies the certificate's hash, and Chrome caps such certificates at 14 days. Section "Certificates" below is the
consequence.

### Sans-IO netcode

`crates/digblock-net` has no sockets and no async runtime. Bytes cross one trait, `NetTransport`, with every method sync
and non-blocking, implemented by `digblock-net-native` (wtransport on a detached thread with a current-thread tokio
runtime) and `digblock-net-web` (hand-written WebTransport bindings), plus an in-memory loopback pair for tests. The
trait is `Send` off-wasm and deliberately not `Send` on wasm32, because the browser handle is a JS object.

The protocol is edits and snapshots, never terrain. Both sides generate the world from the seed, which is why the sim
version must match exactly. Wire types use postcard with positional discriminants, so enum variants and struct fields
are append-only. The 8-byte preamble (`DIGB`, u16 version, two reserved bytes) is frozen forever and precedes any
length-prefixed frame on the control stream in both directions, so any two builds can always exchange versions and
refuse each other cleanly instead of hitting a decode error on a differently shaped Hello. The server always answers
with its own preamble, even when refusing, so a rejected client can report which side must update.

Datagrams are one message per QUIC datagram with no length prefix. Reliable frames are u32-length-prefixed, and the
server holds clients to a 4 KiB frame while accepting a 1 MiB frame from the server, refused from the length header
alone before any body is buffered. Inputs go out at 30 Hz with the last four unacked ticks repeated in every datagram,
and snapshots come back at 20 Hz, tiered by distance so far players update at a quarter rate.

Two client-side details worth copying:

- **Poll budget in events, not frames.** A journal column frame expands to one event per cell, so a frame-count budget
  bounds nothing; a few hundred columns of join sync is six figures of events materialised inside one rAF callback.
  Reliable bytes are always drained from the transport, but decoding is budgeted and leftovers stay in the frame reader.
- **Prediction compares against the sent claim, not the live post-state.** A replay rewrites the buffered states, but
  the server adopted the original in-flight claims, and judging its echo against the rewritten values re-replays every
  snapshot. At rest that ping-pong never decays and vibrates the camera at RTT period.

### WebTransport bindings by hand

`crates/digblock-net-web/src/transport.rs` declares the WebTransport surface itself, because web-sys's version sits
behind the global `web_sys_unstable_apis` cfg, which would poison every wasm build command for a modest amount of glue.
Three rules govern the bindings:

- **`catch` on anything a browser may not ship.** `getStats` is in the spec but unimplemented in Chrome, still absent in
  151, so calling it throws rather than returning a rejecting promise. Without the catch, that throw unwinds out of
  `poll` and out of the rAF handler, which is never re-armed, and the game freezes. Feature detection is latched on the
  first throw so the same error is not caught twice a second for the life of the session.
- **Typed getters in hot paths.** Reading `done` and `value` off a read result through `Reflect::get` interned a fresh
  JS string on every read, which surfaced as render-thread pauses.
- **`Reflect` for every structural hop**, precisely because browsers ship the API in parts: the session can exist while
  `datagrams` is `undefined`, and reading `.writable` off `undefined` throws inside the driver task, lands in no
  `Result`, and the connection just expires at the connect timeout with no diagnosis.

Backpressure is read off the WritableStream's `desiredSize`: a datagram is dropped at a measured backlog, logged once,
because a stalled connection otherwise piles up JS-side writes nothing above this layer can see. Reliable sends are
never dropped; the browser buffers internally, matching the native driver's unbounded channel.

Error reporting is built for a device where attaching a console is hardest. A `WebTransportError`'s message is routinely
empty, so `source` and `streamErrorCode` are read out. A stream rejection is parked for 250ms so the session's own
`closeCode` and `reason` can win the race on a real teardown. A stall watcher fires at 70% of the connect timeout and
names the pending stage, because a rejection names itself but a promise that never settles does not.

### Certificates: the 14-day cap

A browser can trust a self-signed QUIC server only through `serverCertificateHashes`, and Chrome caps such certificates
at 14 days of validity. That single rule decides the deployment:

- There is no reverse proxy, no ACME, no Let's Encrypt on the game port. The server terminates QUIC/TLS itself with a
  self-signed certificate.
- `apps/server/src/net.rs` mints a 10-day certificate (wtransport's default of 14 sits exactly on Chrome's cap, and
  Chrome rejects at the boundary with any clock skew), rotates it every 7 days in-process via `reload_config` without
  rebinding the socket, and re-registers the new hash with the master. Joins racing the swap see a stale pin for one
  HTTP round trip and simply retry.
- The master publishes the hash with each server listing, and the join token response carries it too. The native client
  pins the same hash; the master's own health probe validates it in C# with a fixed-time compare, so all three sides
  express one trust policy.
- A malformed hash is a hard error on both clients, never a downgrade to insecure. `?master=` rides in shared links, so
  one bad string from a hostile master must not become a session with certificate validation switched off.
- The server binds IPv4-any rather than dual-stack, because Docker Desktop's UDP port proxy fails to deliver to a
  dual-stack bind in-container. The port block is mirrored across the C# master, the deploy files and the fleet tool,
  and a test reads those files from disk to keep them honest.

### What browsers lack, and what the code does

| Capability                                            | What happens                                                                                                                                |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| No `WebTransport` constructor (Firefox, old Safari)   | Immediate named disconnect. There is no WebSocket fallback anywhere in the repo.                                                            |
| `datagrams` present but no writable side (iOS Safari) | The datagram-less path below. First-class, not degraded.                                                                                    |
| `datagrams` entirely undefined                        | Same path, reached through `Reflect` so nothing throws.                                                                                     |
| `getStats` missing (Chrome)                           | RTT reads as dashes in the overlay; server-list ping falls back to handshake time divided by three round trips and is flagged estimated.    |
| Empty error messages                                  | `source`, `streamErrorCode`, `closeCode`, `reason` are read out and named.                                                                  |
| Page unload                                           | `close()` queues the stream close without awaiting; there is no later task for it to resolve in. The server's idle timeout is the backstop. |
| A phone reading slowly                                | Snapshots are skipped, never refused, so the session survives.                                                                              |
| No loopback listener for OAuth                        | The game page is the redirect target; see "Sign-in".                                                                                        |

### The datagram-less path

iOS Safari ships `WebTransport.datagrams` and gives it no writable half. Every input the client sent went nowhere, and
every snapshot the server aimed back arrived at a channel that did not exist: a client that reached the handshake and
could never play. That is a missing API rather than a degraded network, so it is answered as a capability, not a
condition.

- The transport probes both datagram halves before it reports `Connected`, because the answer rides the Hello that
  `Connected` triggers. Either half missing takes both down, because the fallback is a pair of directions.
- The client announces `ClientMsg::NoDatagrams` immediately after Hello on the ordered control stream. Announced, never
  inferred, or the server would keep answering on the absent channel until the client happened to speak.
- Unreliable traffic then rides the reliable stream in both directions inside `Unreliable(Vec<u8>)` wrappers. The bytes
  are identical to the datagram encoding and decode at the same site, so there is no second shape of an input batch for
  the two paths to disagree about. Three appended variants; the protocol floor did not move.
- **A wrapped snapshot stays lossy.** A datagram leaves the server's per-client queue the instant the pump reaches it,
  while a wrapped frame leaves at the speed of one stream's flow control, so a phone reading slowly parks them there.
  The queue then fills with snapshots and the next edit, chat line or teleport is refused, and a refused reliable frame
  marks the client dead. The session ended over the one message class that has always been allowed to be dropped,
  seconds after every join. `apps/server/src/sim/sync.rs` skips a wrapped snapshot when the queue is past half capacity,
  and a skip is a policy rather than a refusal, so the stall reaper does not count it.
- `?nodatagrams` forces the path on any browser, because the one browser that needs it is also the hardest to attach a
  console to. A path with no way to run on a dev machine rots.

### Server-side backpressure and admission

The server runs one authoritative fixed-tick sim on one thread and emits pre-encoded messages onto bounded per-client
channels of 512 slots. A peer that stops draining is a dead peer, not an unbounded heap: `try_send` failure on a
reliable frame marks the client dead. Broadcasts encode once into an `Arc<[u8]>` and fan out by refcount. Join sync is
budgeted at 16 frames per tick with 64 slots kept in reserve so backlog can never starve edits or chat. Snapshots are
the one lossy class: a full datagram queue is tolerated, and only a queue full for five seconds marks the client dead.

Admission has a hard 20 second ceiling on everything before the sim hears about a connection, including the wait for the
control stream to open, because the keep-alive interval means the idle timeout never fires against a peer whose QUIC
stack merely ACKs, and a wave of frozen tabs would otherwise wedge the server against its connection cap. A per-IP
connection cap exists because the global cap alone let one peer force an outbound token verify per slot, turning the
server into an amplifier against its own master. Reliable reads pause when a per-client message budget is exhausted, so
QUIC flow control backpressures the sender instead of the server decoding a flood.

### Sign-in

The master is the authorization server as far as the game is concerned. Players sign in with Google, Facebook, or
Microsoft through stock ASP.NET OAuth handlers (`apps/master/Auth/AuthEndpoints.cs`), and the game client never sees a
provider token. Scopes are trimmed to the subject only; no email or real name is stored, because the provider subject
alone re-identifies a returning player. A provider with no configured client id is simply absent from the chooser, which
is the feature flag.

The client-facing flow is PKCE with a one-time code, in three requests:

1. `GET /auth/start` with `redirect_uri`, `state`, an S256 `code_challenge`, and `platform`. Everything is validated
   here, so a client that forgot PKCE fails immediately rather than at redemption. The browser is bounced to the
   provider and back to `/auth/complete`, which consumes the pending record, signs the external cookie straight back
   out, mints a code, and redirects to `redirect_uri?code=&state=`.
2. `POST /api/session/exchange` with the code and verifier. The code is stored hashed and removed before the verifier is
   checked, so a wrong verifier burns it rather than letting someone who read it out of a proxy log keep guessing.
3. `POST /api/session/refresh` at every startup, which rotates the token. A rotated token replayed later is the one
   security event: a second copy exists and the master cannot tell thief from victim, so the whole chain dies and both
   sign in again. Signed-out or expired tokens are refused quietly, indistinguishably.

Every JSON endpoint is bearer-authenticated, never cookie-authenticated. That separation is load-bearing: the browser
pages under `/auth` use a cookie, the API does not, and so the any-origin CORS policy the web client needs stays legal,
because any-origin plus credentials is not.

**Redirect rules** (`apps/master/Auth/RedirectUris.cs`) are what keep the master from being an open redirector. Exactly
two shapes are allowed: an `http` loopback IP literal on any port, per RFC 8252, because a native client cannot reserve
a port in advance; and a hardcoded list of owned web origins, matched on origin only because a path is not a trust
boundary. A target carrying its own query or fragment is refused, since the master appends its own. `localhost` is never
accepted, because it resolves through the host's name service. This is the second reason to open the dev server as
`127.0.0.1`.

**The web client** (`apps/web/src/auth.rs`) cannot listen on a port, so the game page itself is the redirect target, as
a top-level navigation rather than a popup, because a popup opened from the engine's command drain is one task removed
from the click and that is exactly what a popup blocker stops. The PKCE verifier, the state, and the page's original
query go into `sessionStorage`; the resulting session goes into `localStorage` under one key. Randomness comes from
`crypto.getRandomValues` and fails rather than falling back to `Math.random`, because an all-zero verifier makes a
leaked code redeemable by anyone. On return, `take_callback` scrubs `code` and `state` from the address bar with
`history.replaceState`, puts the saved query back minus one-shot actions like `join`, and redeems the code before the
engine exists so the menu opens already signed in. It must run before any other query param is read, or `?master=` is
lost and the code is redeemed against production instead of the master that minted it.

**The desktop client** (`apps/desktop/src/auth.rs`) binds `127.0.0.1:0`, opens the system browser, and runs an accept
loop rather than taking the first connection, because a browser happily asks for `/favicon.ico` first. A state mismatch
aborts the whole sign-in rather than waiting for a better callback: something else aimed at our port. The session file
is written with mode 0600.

**Token isolation.** On both launchers a single services task owns every HTTP call and is the only place the session
token lives. The engine's menu holds an account id and a display name, both public, and CLAUDE.md states the invariant:
the engine never does HTTP and never sees the session token. On desktop the sign-in gets its own thread, because the
services loop is a blocking receive and a sign-in waits on a human for up to three minutes.

**Dev sign-in.** `POST /api/dev/signin` mints an account from any subject string. It is enabled only in the Development
environment, and setting the option anywhere else is fatal at boot, because a dev escape that silently does nothing in
production is how a dev escape ends up live in production. Account ids are derived from the subject by hash, so the
justfile can hardcode the admin id before anyone has signed in.

### The join sequence

1. **Browse.** `GET /api/servers` is public and returns name, URL, certificate hash, player counts, and the server's
   protocol and sim versions. Decoding is lenient in the right direction: a bad player count clamps, because one bad row
   must not hide every server; a bad version clamps to incompatible, never to ours.
2. **Filter.** `compat()` in `crates/digblock-net/src/protocol.rs` is asked by the row that draws the server, by the
   action that would join it, and by the launcher before it spends a ping probe, so nothing offers what the handshake
   would refuse. The row says which side is too old.
3. **Mint.** `POST /api/join` with the server id and the bearer session. The master checks the ban, requires a display
   name (a name nobody chose is a name nobody owns), and returns a 16-byte token as hex, the URL, and the certificate
   hash. There is deliberately no display name in the request: the master owns the name, and removing that field is what
   closed admin impersonation. Tokens live 45 seconds.
4. **Connect.** WebTransport with `serverCertificateHashes` on the web, a pinned SHA-256 policy natively. A token that
   is the right byte length but not ASCII is rejected rather than sliced, because that string arrives through a shared
   link.
5. **Preamble, then Hello.** The client sends the frozen preamble and a Hello carrying the token, a claimed name, and
   its draw distance. Since protocol 23 there is no client-chosen identity in the Hello at all.
6. **Verify.** The server refuses an all-zero token, refuses if it has not registered yet, checks a local replay set so
   two connections racing one token never reach HTTP, then calls `POST /api/tokens/verify` with its server key. The
   master identifies which server is asking from the key, so a token minted for another server never verifies, and
   unknown, expired, consumed, and wrong-server are indistinguishable. The response carries the display name, account
   id, and skill totals; the server overwrites the claimed name with the master's answer and carries the account id on
   the connection rather than the wire message, so no future reader mistakes it for something the peer claimed. The
   master being down means nobody gets in, which is fine, because nobody could mint a token either.
7. **Welcome.** After the ban gate (account and IP, either enforces) and a duplicate-account check keyed on what the
   master said, the server sends seed, player id, spawn, tick, time of day, and terrain version, then streams the join
   sync journal columns.

### Server registration and heartbeat

`apps/server/src/master.rs` registers on boot with its port, certificate hash, name, and optionally an advertised host,
and retries forever with backoff from 10 seconds to 5 minutes. The response carries a server key that is persisted to
`data/master-key` with mode 0600 and sent as a bearer on every later call. The registry entry's id is stable across
restarts, so a lost data directory does not change a server's identity.

The master's registration handler is its most attacked endpoint, and the probe order is the security property. The
source address is probed first, because that is the one piece of evidence an attacker elsewhere cannot manufacture. A
differing advertised address is probed too. A claimed, live address may be re-registered only by the key holder or by a
registrant passing the source probe; anyone else is refused and temp-banned with a doubling ladder. Public-address
checks run after DNS resolution, never on the string, because a hostname resolving to a private address is the whole
SSRF trick. The probe host used to be a request field and was removed, because it let an anonymous caller choose the
only address the master ever dialed while clients were handed a different one.

Heartbeats go every 30 seconds. A 204 resets the strike count; a 404 means the master lost us, so the server drops its
registration and registers again. The master de-lists a server silent for two minutes before spending a probe on it,
then re-probes the rest with three strikes allowed. Certificate rotation signals the registration task, which re-reads
the hash on every attempt so a rotation landing mid-retry still registers the current certificate. Graceful shutdown
sends a delete.

Server names are sanitized rather than rejected, because registration must not fail over cosmetics; a self-hoster would
see an unreachable-style mystery instead of a validation hint.

## 9. Deployment and release

Three independently deployed artifacts, three targets, one version number. Every deploy is a manual workflow dispatch;
CI is disabled and `just gate` is the gate.

| Artifact          | Built by                    | Where it runs                                                      | Trigger                                |
| ----------------- | --------------------------- | ------------------------------------------------------------------ | -------------------------------------- |
| Game server image | `docker/Dockerfile.release` | `ghcr.io/zaneris/digblock-server`, pulled by VPSs and self-hosters | `.github/workflows/release-server.yml` |
| Master            | `apps/master/Dockerfile`    | GHCR, then an Azure Container App named `digblock-master`          | `.github/workflows/deploy-master.yml`  |
| Web bundle        | `just build-web`            | Cloudflare Pages project `digblock`, serving `digblock.net`        | `.github/workflows/deploy-web.yml`     |

### The game server image

`docker/Dockerfile.release` is a three-stage cargo-chef build producing `linux/amd64` and `linux/arm64` (a Raspberry Pi
on a 64-bit OS is a supported host). The build stages run on the build platform and cross-compile for the target, so
rustc never runs under QEMU; only the runtime stage's package install does. The runtime image ships the server binary
and CA certificates (it speaks HTTPS to the master) and nothing else, with `/data` as a volume and `4433/udp` exposed.

Three build gotchas are recorded in the Dockerfile and `.dockerignore`:

- Build and runtime stages must ride the same Debian suite. A binary linked against a newer glibc demands symbol
  versions the runtime lacks, and it fails only at container start.
- `libc6-dev-arm64-cross` must be named explicitly under `--no-install-recommends`, or `ring`'s C build dies on a
  missing header.
- `rust-toolchain.toml` must stay out of the build context. If rustup sees `channel = "stable"` it treats it as a
  different toolchain from the image's, reinstalls mid-build, breaks the aarch64 target, and discards the chef cache.
  Ignore patterns must be both anchored and recursive.

### How a VPS runs it

`deploy/compose.yml` is the whole self-hosting kit:

```yaml
x-port: &port "4433"
name: *port
services:
  digblock:
    image: ghcr.io/zaneris/digblock-server:latest
    restart: unless-stopped
    ports:
      - {target: 4433, published: *port, protocol: udp}
    volumes: [./data:/data]
    environment:
      DIGBLOCK_ADVERTISE_PORT: *port
      DIGBLOCK_SERVER_NAME: "My DigBlock World"
```

One YAML anchor is simultaneously the published host port, the advertised port, and the compose project name, so two
folders can only collide when they could not both run anyway. The container always binds 4433 inside. The port must sit
in the block 4433 to 4499, which the server checks before anything binds, so a misconfiguration fails locally rather
than as a rejection after registering. `./data` is the whole backup: one journal directory per seed, sharded by region,
plus inventories, spawn, waystones, `bans.json`, and `master-key`.

Upgrade is `docker compose pull` and `docker compose up -d`. A save-format break never replaces a world: the server
refuses to start and names the file it left untouched. Two separate levers exist, one to archive an unreadable world and
one to adopt a new generator on a readable one, deliberately not shared so a switch set to recover an unreadable world
cannot later trade away a perfectly good one.

### The first-party fleet

`tools/fleet/fleet.py`, stdlib only, driven by `just fleet`. Its inventory file `fleet.toml` is authoritative: a
server's compose file on its host is the deploy kit with the row's values substituted into its slots, so the kit stays
the single template and a kit change reaches every server on the next apply. At the time of writing the fleet is two VPS
hosts running three servers between them, each server a folder under the operator's home directory with its own port.

The version at the top of `fleet.toml` is the fleet pin, and it becomes the image tag. A server row may carry its own
version override, which is a deliberate pin the release workflow never touches.

`just fleet apply` diffs, refuses a port or seed change without an explicit flag, warns every run while a world lever is
armed, stops the container, backs up `./data` to a timestamped archive keeping the last five, pushes the compose file to
a temporary name and moves it into place so a half-copied file never sits where `up` reads it, pulls, brings the
container up, and then watches the log to a verdict for two minutes. The verdict requires both "registered with master"
and "listed at", because a player reconnecting mid-upgrade logs a refusal that contains the first phrase, and matching
on it alone reported success and stopped the watch. `just fleet status` cross-references the master's server list and
marks drift between running and pinned. The fleet tool has its own unit tests in `just check`, because a boot verdict
read out of a log is a string match against a program that changes, and nothing else in the gate would notice it
drifting.

### The master on Azure

The master image is .NET 10 on Alpine with `libmsquic` installed, because Linux plus msquic is the trodden path for
`System.Net.Quic`, and the container has to probe game servers over real QUIC. Deploy is a build and push to GHCR, then
one command against the Container App with the image addressed by its immutable commit hash, never `latest`. The
revision therefore records exactly what is running, and rollback is activating the previous revision. There is no
infrastructure-as-code in the repo; the Container App, the Entra federated credential for the workflow, and the storage
mount are one-time manual setup documented in the workflow header.

Accounts live in SQLite at `/data/master.db`, and the Container App filesystem is ephemeral, which makes three things
load-bearing:

- **An Azure Files share mounted at `/data` with `nobrl`** in the app template's volume mount options. Without it, CIFS
  forwards SQLite's byte-range locks to the SMB server, whose lock-upgrade sequence fails as "database is locked" on the
  first write. The new revision dies at migration, the rollout keeps serving the old image, and nothing in the deploy
  output says so.
- **Exactly one replica.** SQLite is single-writer; two replicas over SMB is corruption. It is also what makes `nobrl`
  safe.
- **WAL forced back to DELETE in code.** EF Core's provider turns on write-ahead logging itself, which needs a shared
  memory file and is unsupported over SMB, so an interceptor resets the journal mode on every connection open. It is
  unconditional, because a mode that differs between dev and production is a mode nobody tests.

Data-protection keys are persisted beside the database on the same volume, otherwise a redeploy mid-sign-in rejects the
OAuth callback with a correlation failure. Migrations run at boot so the deploy has no forgettable step. OAuth client
secrets are Container App secrets surfaced as environment variables, never in a settings file, and a provider with no
credentials simply does not render a button. The client redirect origins are hardcoded rather than configured, because
config forgotten on a deploy fails sign-in on the web alone.

### The web bundle on Cloudflare Pages

The workflow adds the wasm target, installs `just` and a pinned prebuilt `wasm-bindgen` that must be kept in step with
`Cargo.lock`, runs `just build-web`, and deploys `target/web` with `wrangler pages deploy`. There is no wrangler config
file; the CLI arguments are the whole configuration. The workflow runs `just build-web` itself rather than a mirrored
copy of its steps, so the deploy cannot skew from a local build, and the recipe's own version gate turns a stale pin
into a loud failure in CI rather than a runtime link error.

The HTML and wasm coupling that makes this safe is in section 2 in full. In deploy terms: the two module pairs are
content-addressed by their bytes and the bindgen version; only `index.html` knows the stamps and it is never cacheable;
`/pkg/*` is declared immutable for a year; a real `404.html` stops the edge from caching HTML under a stamped script
name; and the page retries a failed pair once under a query salt, which has been seen live when the first request after
a deploy landed before the asset had propagated. The `public/` directory (favicons, manifest, robots, the 404 page)
lands at the deploy root and is mutable by name, unlike `/pkg/`.

There is no rollback mechanism for the web bundle in the repo. It is a redeploy, or Cloudflare's own deployment
rollback.

### The release process

There is no release recipe in the justfile. A release is the `release-server` workflow run by hand with a version
string, and it runs three jobs in a fixed order:

1. **Bump.** The version is validated against a strict pattern before anything is touched, because this job commits to
   `main` and a typo must not become a commit. Four anchored edits follow: the workspace version in the root
   `Cargo.toml` (every crate inherits it, so a new crate is covered the day it is added, and a per-crate list was
   removed because a list of crates to remember is a list somebody eventually does not), the iOS bundle version, the
   fleet-wide pin in `fleet.toml` scoped to the lines before the first host header so a row-level override survives, and
   a lockfile update. The commit is `chore: release v<version>` by the actions bot, pushed to `main`.
2. **Image.** Checks out the bump commit, builds both architectures, and pushes the version tag and `latest`. The order
   matters because binaries bake the crate version at compile time and log it at boot, so a release built from an
   unbumped tree would report the wrong version forever.
3. **Release.** Creates the `server-v<version>` tag against the bump commit with the compose kit and its README
   attached.

The fleet pin moving in the bump job is what turns an update into `git pull` and `just fleet apply` with no edit
between. Before that change every release ended with a hand edit to `fleet.toml`.

Rollback for the fleet is editing the pin back and applying again: the image tags are immutable per version, so the old
tag is still there, and apply stops, backs up, pulls the older tag, and watches the boot. Self-hosters pin their own
compose file to a prior tag. Git history is the release log.

### Version compatibility across a rollout

Four version axes, each with its own rule:

| Constant           | Rule                                                                                             |
| ------------------ | ------------------------------------------------------------------------------------------------ |
| `PROTOCOL_VERSION` | Bump on any change to a message's shape or meaning. A window down to `MIN_PROTOCOL` is accepted. |
| `MIN_PROTOCOL`     | Rises only when a field changes inside an existing variant; appended variants never move it.     |
| `SIM_VERSION`      | Exact match. Terrain is never sent, so divergent worldgen is silently different worlds.          |
| Journal `VERSION`  | Always attempt backward compatibility; a hard break needs explicit approval.                     |

There is no server-before-client ordering encoded anywhere. Instead the compatibility question is answered in the menu,
which makes ordering not matter. Servers report their protocol and sim versions at registration, the master publishes
them in the server list, and the client decides joinability from the list before spending a token or dialing anything.
An incompatible server stays listed, because a self-hoster's players must see that it is there, but it is never an
offer: its row says "Server too old" or "Client too old", selection skips it, and the scripted join path refuses too.
The frozen preamble guarantees that any two builds can still exchange versions and refuse each other cleanly.

So the web bundle and the fleet are deployed by two separate workflows from the same version, and in the gap between
them the mismatched side is visible and labelled. Self-hosted servers on `latest` follow on their own pull; ones pinned
to a version show as too old until their operator upgrades. The master is version-agnostic on this axis and never needs
to roll in lockstep; its own compatibility surfaces are the port block, pinned by a test that reads the C# file, and the
list of owned redirect origins.

### The local rig

`docker/compose.yml` is the development stack, not production: a master in the Development environment with the dev
escapes enabled (dev sign-in, private probe targets, a probe host override), a server image that also carries the bot
binary and `tc`, and a bots service behind a profile with a raised per-IP connection cap because every bot shares one
address. The entrypoint applies a netem profile from an environment variable, and the justfile defines `clean`, `wifi`
(30ms, 1% loss), and `cellular` (80ms with 20ms jitter, 3% loss). The UDP port mapping is mandatory; QUIC never touches
TCP. The documented debugging rule: if a client times out, read the server's logs first, because a server that crashed
at boot produces the same client-side timeout a dead network would.

## 10. Skills, XP, and hiscores

A character is one character on every server. Levels are earned on a game server, persisted there per account, pushed to
the master's global board by servers the master trusts, and pulled back down by any server a player joins next. The
design is shaped by one rule: nothing on this path can ever lower a total, and nothing a client says about its own
levels is believed.

### The model

`crates/digblock-core/src/skills.rs` defines three skills as an append-only `#[repr(u8)]` enum, because the discriminant
is both a wire and a persistence format. Storage is XP only: a `SkillSet` is one `u64` per skill, and level is always
derived, never stored. The curve is a pinned integer table of XP-to-next per level up to 99, not a formula, on the
stated ground that floating-point rounding is not a foundation two languages must agree on.

Three mutators with distinct contracts:

- `add_xp` saturates and returns the new total, which is what an XP frame carries.
- `set_total` takes the max of the stored and incoming totals, so a stale or reordered frame can never rewind a newer
  one.
- `merge_from` takes the per-skill max of two sets, never the sum, so a legacy character folded into an account is not
  paid twice for one career.

There is a `clear`, and it is deliberately reachable from no award path and no wire message; only a versioned
persistence migration may move a total down.

XP awards are pure functions of what was done, computed identically by the server and by the offline engine, with
integer-only tables: a step ladder per block worth, a depth ramp from sea level to bedrock, a reduced rate for the extra
cells of a wide pick swing, fixed pairs for drawing from an essence field, craft worth for alchemy, and distance-capped
awards for teleports and priced casts. One entry point, `dig_award`, wraps the mining tables so no award site can read
them and forget the depth ramp. The dig-speed and decay tables are covered by `SIM_VERSION` because client prediction
and the server's completion gate run them independently; the XP curve and award tables are not, and are pinned instead
by tests.

Mastery is a level threshold in `unlock.rs`, packed as one bit per skill for the wire; a bit past the roster decodes to
nothing, because a newer server's fourth skill is not a reason to draw a glyph this build cannot name.

### Persistence on the game server

`apps/server/src/player_store.rs` holds one record per account in a single file rewritten atomically at most every five
seconds on a dedicated writer thread. The record carries inventory, layout, position, teleport cooldown, skills, and the
spell bar. Skills are written as a count and then nonzero pairs of skill index and total, the same shape as the wire.

Every version the store has ever written stays loadable. Frozen `FIRST_*_VERSION` markers trigger on-read migrations by
version, never by content: a record older than the account era lands in a quarantine keyed by the old device hash, a
record from before a redesign has certain items struck, and one migration winds a single skill back to zero because
those totals held milestone XP the current ladder cannot produce. A test loops over every version and asserts each
migration's effect. The store refuses only files newer than itself.

The store's `put` is also the reporting hook. When a record's skills changed and are not all zero, the new totals go
into an outbox. At boot, every stored non-default record is replayed into the outbox, so a board that has never heard of
this server catches up without anyone having to play first. The award sites never touch the outbox; the store does.

### Pushing to the master

`apps/server/src/hiscores.rs` is the outbox: a map from account to `SkillSet`, totals never deltas, so a row is
idempotent, a failed post is simply put back, and a newer push supersedes an older one. Restoring failed rows uses
insert-if-absent so a restored row never covers a newer push.

Reporting rides the heartbeat in `apps/server/src/master.rs`, only behind a heartbeat that landed, one batch of up to
500 rows per beat so a burst of level-ups drains over a few beats rather than in one oversized post. Totals are clamped
to a signed 64-bit value before sending, because the master stores a signed total and one saturated unsigned value would
fail the decode and take the whole batch with it. The outcomes:

| Response | Meaning            | Action                                                                            |
| -------- | ------------------ | --------------------------------------------------------------------------------- |
| 204      | Accepted           | Log the count.                                                                    |
| 404      | The master lost us | Restore the rows, drop the registration, register again.                          |
| 403      | Untrusted          | Log once, stop reporting for this registration, drop the rows. Re-asking is spam. |
| Other    | Transient          | Restore the rows, retry next beat.                                                |

Whether a server may report at all is the master's verdict, carried in the registration response and fed into the sim as
an event from inside the registration code, so no path that lands a registration can forget to tell the sim. A lost
registration carries no verdict; the last one stands.

### The master's board

The master stores one row per account and skill: XP, the server that last raised it (diagnostics only), and a timestamp.
The write is a monotone max: a lower figure is ignored, not written, matching the game's own `set_total`, so a server
holding an older copy of a character cannot rewind the one that moved on. Unknown accounts are skipped, never created.
An unknown skill index or a negative total refuses the whole batch, so the roster is extended on both ends or neither.

The master mirrors the level curve in C#, and the Rust side is the definition. A Rust test reads the C# source as text
and asserts the table, the max level, and every enum row match, because nothing in either build system can check one
against the other.

Boards are computed in memory per request: ordered by level, then XP, then account id for stable ties. The overall board
sums derived levels across all skills, so an unreported skill counts as level 1, as the game would say. A player's own
ranks come back only for boards they appear on; unranked is a missing row, not a zero. Reads are deliberately uncached,
with a note to revisit when a request shows up in the logs.

Endpoints: a server-key-authenticated report route, a public JSON board with page and size clamped to 25, a
session-authenticated "my ranks" route, and a script-free server-rendered HTML page whose tabs and pager are links.
Reports are rate limited per hashed server key, lookups per address.

### Trust

Token verification proves a server is real, not that its sim is honest, and any registered server could report inflated
totals. So the global board takes reports from a hand-kept list of trusted servers and nobody else, matched on the
probe-verified address the server is listed at, so being trusted needs the machine at that address rather than knowledge
of its name. The shipped default is an empty list: no server writes and the board stays blank. The master publishes the
verdict in the server list, so a player can see which servers are ranked before joining, and the client tells them at
the door of an unranked one that XP earned there stays there. That message is decided by the client from the master's
listing, whatever the server itself might say.

What a self-hosted, untrusted server can and cannot do:

- It can grant any local level it likes; its own store is its own.
- Its XP stays local. The refusal is final for the registration and the outbox is dropped.
- It can still pull the board's totals when a player joins, because only the write is gated, but the fold is a max, so
  nothing it invents propagates and nothing it holds can reduce what the board has.
- Once a server learns it is ranked, the testing levers (`/give`, `/reset`) leave the admin help, are re-advertised to
  every online admin as withdrawn, and are rejected at dispatch from the same table, so a typed command the
  advertisement never offered still cannot reach them.

The client is never trusted for XP. Awards are computed at three server chokepoints (dig completion from the journaled
previous cell, a successful alchemy operation, and priced casts including teleports), each following the same pattern:
add the XP, send the new total, persist the record, and announce a level-up to everyone but the author. The account id
arrives from the master's verification response rather than the client's Hello, precisely so the client cannot choose
it.

### Pulling at join

The master's token verification response carries the account's totals, and any server may pull them, trusted or not. In
the server's handshake, the pulled set is merged into the stored record by max, and the record is written back only if
something moved, because the store's `put` is the outbox hook and what came from the board need not go straight back to
it. The server flag `--use-master-levels`, default on, gates this pull alone; the push is gated only by the master's
trusted list.

Join sync then always sends a full `Skills` frame, even an empty one, immediately after the inventory and layout frames.
That ordering is load-bearing: a client-side arming step re-evaluates at the skills frame against an inventory already
adopted.

### On the wire and on the client

Three server messages: a full `Skills` sync, an `XpGain` carrying the new total rather than a delta so it is idempotent
and reorder-safe, and a `LevelUp` broadcast to everyone but the author. Mastery bits ride in the remote player state so
nameplates can draw a glyph, appended in a protocol bump that raised the floor because it changed the shape of an
existing struct.

On the client, XP is explicitly not predicted. A full sync is adopted wholesale with no toasts, because that is state
arriving, not an award happening. Every award, online or offline, goes through one presentation function that bails if
the total did not rise, shows the delta, and on a level cross walks every crossed level for its unlock lines. Offline
awards use the identical core tables and the same function, so presentation cannot drift between the two modes.

The hiscores screen is a pure model with no I/O: it emits fetch commands, drops any answer whose board and page are not
what is in view, clamps paging against the page size the master actually cut rather than the one asked for, and keeps
the previous page's shape while a fetch is pending so a tab change does not collapse the panel. The launcher's services
layer marks the player's own row, because it holds the account id and the engine deliberately never sees it. Ranks are
refetched on every open of the character screen and boards on every tab or page change, with no cache.

### Gotchas

- Appending a skill means editing the Rust enum and the C# mirror in one change, or the mirror test fails and a report
  carrying the new index refuses the whole batch on the master.
- The outbox never reports a reset to zero, and the master never lowers a total, so there is no path that lowers a board
  figure short of a database edit. That is intended.
- A refusal is sticky per registration. Adding a server to the trusted list needs a re-registration before totals flow.
- The player store evicts least-recently-used records past ten thousand identities. Skills survive only because the
  board holds them and the join pull restores them, unless the server has turned the pull off.
- A version-triggered wind-back on the server runs once, but the board may still hold the higher figure, and the next
  join on a pulling server brings it straight back.

## 11. Persistence and settings in the browser

Both use `localStorage`, and the choice over IndexedDB is deliberate: `flush` runs inside the synchronous `on_suspend`
contract, which may be the last callback the process gets, and IndexedDB is async.

Worlds (`apps/web/src/store.rs`) are sharded by region, each shard holding base64 of the exact server-format journal
bytes, so only changed regions are rewritten and never the whole edit history through a synchronous in-frame `setItem`.
Writes are throttled to two regions per maintain call every five seconds; `flush` ignores the cap. Dirty regions clear
only on commit, so a refused write retries rather than silently dropping edits. Quota or private-mode failure sets a
degraded flag, warns once, and play continues from memory. Migration from the older single-blob layout removes the
legacy key only after every shard commits, so an interrupted migration re-runs from the intact original. Walking the
keyspace for a prefix is fine at open time and never on the save path, because `localStorage` has no prefix scan.

Settings (`apps/web/src/settings.rs`) are one JSON key. The engine's settings type is serde-free; each launcher owns a
mirror struct with every field `Option<T>` and `#[serde(default)]`, so absent or malformed fields fall through to
platform defaults. Key bindings are stored by stable name and unknown names are skipped on read, so a file written by a
newer build loads on an older one and vice versa.

## 12. Lifecycle

`crates/digblock-platform/src/lifecycle.rs` is a pure state machine: events in, an ordered list of actions out, no
winit, no surfaces. Half the file is tests, including one that walks every phase and event pair.

Mechanics worth stealing:

- A generic fatal acquire error is treated as loss-suspect and retried a bounded number of frames, because a dying
  device surfaces that way before wgpu's lost callback is pumped.
- Occlusion is distinct from timeout and never runs out of patience, because it clears when somebody moves a window,
  which may be never. Retrying as fast as the CPU allows would burn a core on a covered window.
- Device loss while suspended is deferred, and on resume the actions come back in order: recreate the surface, then
  reinitialise the device, because there is nothing to recover into otherwise.
- A surface format that changed while backgrounded forces one renderer rebuild; pipelines built for another format make
  every draw a validation error.

On the web, winit splits `pagehide` by `persisted`: the back-forward-cache case reaches suspend, a real close reaches
exit. Suspend on wasm also runs the exit path once, because the page may be restored but the connection will not be, so
the client says goodbye rather than leaving the server holding a dead session. Without a `Bye` the server waits out
QUIC's idle timeout, and with one account holding one live connection, the player being held is the one trying to get
back in.

## 13. Testing the web build

- `just check-web` runs wasm32 clippy on the four web-reachable crates and then every shader through real Chrome. No
  wasm-bindgen CLI is needed for that half.
- `just web-e2e` drives the real bundle through Playwright against the installed Google Chrome, single worker because
  the browser holds the GPU and two pages rendering turn every frame-count deadline into a coin toss. The static server
  refuses to start without a built bundle, because a stale bundle tests stale code. Failure reporting reads the page's
  lifecycle events and the overlay's status text, so "wasm fetch never returned" and "first frame never presented" do
  not both read as one generic timeout.
- The harness's flag roster comes from the same Rust knobs table, printed by a built desktop binary, because a
  hand-copied list had drifted by two names by the time a reviewer found it.
- The PNG decoder used for screenshot assertions is hand-written and has its own unit suite, pinned separately because a
  decode bug there makes the suite more likely to pass.
- `just gate` is check, check-ios, check-web, and web-e2e together. CI is intentionally disabled pre-deployment.

## 14. Checklist

Things that will cost a day each if learned the hard way:

1. Glue and wasm are one artifact. Stamp them together, and let only one file know the stamp.
2. Serve `application/wasm`, or `instantiateStreaming` refuses the module.
3. A CDN in front of a custom domain may rewrite cache headers per extension. Make the file names immutable instead.
4. Ship a real 404 page, or a missing module comes back as cached HTML under a `.js` name.
5. Open `127.0.0.1`, not `localhost`.
6. Drop frees nothing on WebGPU. Call `destroy()` and replace the bind groups in the same breath.
7. Do not use `create_buffer_init` on wasm. Use `write_buffer`.
8. Do not call `push_error_scope` through wgpu on wasm. Bind the raw device.
9. Raise `max_buffer_size` to the adapter's limit before allocating a large 3D texture, or the screen goes black.
10. You cannot copy into a depth32float texture. Blit with a fullscreen draw.
11. Turn timestamp queries off by default on Chrome/Metal; they halve the frame rate through the power governor.
12. Run every shader through Tint. naga is the permissive end. Parenthesize `*` with `^`, avoid dynamic vector component
    indexing, take derivatives in uniform control flow.
13. Workers: a second wasm module with no `start`, bootstrapped from a blob, transferables both ways, every path
    answers, per-slot readiness, sweeps not seconds, reset by generation counter.
14. Never block on the main thread. rAF plus `setTimeout(0)` is the minimum yield that actually paints.
15. `100dvh`, `touch-action: none`, `overscroll-behavior: none`, `gesturestart` preventDefault, `viewport-fit=cover`.
16. iPhone Safari has no Fullscreen API and no pointer lock. Offer Add to Home Screen and design touch input around it.
17. Fullscreen, orientation lock, keyboard focus, and audio resume only work inside the gesture's own dispatch.
18. Forward the whole text value from a hidden input, not key events. Android IMEs compose plain typing.
19. Chrome has no `getStats`. Wrap every optional WebTransport call in `catch`.
20. iOS Safari has no writable datagram side. Announce it in the handshake and wrap unreliable traffic on the stream,
    but keep wrapped snapshots lossy.
21. Self-signed QUIC certs for browsers live 14 days at most. Rotate in-process and republish the hash.
22. `localStorage` over IndexedDB whenever the write must land inside a synchronous suspend callback.
23. Emulated DPR lies to `ResizeObserver`. Test at native scale before trusting a sizing bug.
24. Deploy the master by immutable image digest or commit tag, one replica, SQLite journal mode pinned to DELETE, and
    `nobrl` on the SMB mount, or the rollout silently keeps serving the old image.
25. Publish protocol and sim versions in the server list so the client refuses from the menu, and no rollout order
    matters.
26. Let the release workflow move every version pin, including the fleet's, so an update is a pull and an apply.
27. Emission never reaches the GI gather; dead probes vote nothing; a toroidal atlas always returns a texel, so guard
    every read outside coverage.
