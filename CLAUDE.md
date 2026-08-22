# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository.

## What this is

Rust + wgpu rewrite of the DigBlock voxel engine (originally Java/libGDX, 2015–2017). The faithful-port goal is
complete; the engine is free to diverge. The old Java repo at `../DigBlock` is a behaviour reference, not a constraint.

**This file is orientation plus the invariants that bite if broken.** It does not argue — the reasoning, the
measurements and the alternatives tried and declined live in the design docs. Read the relevant one before re-litigating
a decision or rebuilding something:

- `GI-DESIGN.md` — lighting: data model, pass order, tuned constants, release notes R13–R35, an honest "What is NOT
  built" list, and `## Invariants`. `## Measuring motion` is the methodology for any lighting capture.
- `BENCH.md` — every perf measurement, including the ones that refuted a hypothesis.
- `SKILLS-DESIGN.md` — skills, XP curve, essences, transmutation catalog, milestones. §14 is the essence redesign and
  supersedes the earlier economy sections where they disagree.
- `AUTH-DESIGN.md` — accounts, sign-in, the master's trust model, the pre-account character migration (§6), a "what is
  NOT built" list, and §5's standing record of the seven trust-model findings.
- `DELVE-DESIGN.md`, `SOCIAL-DESIGN.md` — design-only, not built.

## Four writing rules

**A number lives in exactly one place: the code that defines it.** Comments, doc comments and Markdown name constants
and describe relationships; they never restate what a constant equals, so a retune touches the `const` and nothing else.
Name it, don't quote it ("clamped to `POS_TOLERANCE`"). Describe relationships, not arithmetic — the invariant that
makes a value *right* survives a retune. Consequences are values too: a jump apex, a wade depth, a percentage speedup.
Formats describe structure, not bit offsets; worked examples go in tests. Measurements belong in `BENCH.md`, with the
build, machine and date recorded beside them.

Exempt, because they are identifiers or inputs rather than documented quantities: document and section references,
format versions, names containing a number (`Latin-1`, `wasm32`, `R8Uint`), runnable commands and their arguments, and a
trailing annotation decoding its own literal on the definition line. `BENCH.md` keeps its figures — a baseline with its
numbers removed cannot be compared against. A design doc may quote a value when the value **is** the argument.

**A sentence the player reads lives in exactly one place: `digblock-client/src/voice`.** One `Msg` variant carrying
typed arguments, one template row in `voice::en`, rendered at DRAW time — `Engine::push_feed` takes a `Msg`, so a string
literal cannot reach the feed, and a locale can reorder or re-case what a call site could not. Core NAMES things
(`BlockType::name`, `Skill::name`, `Essence::name`) in ONE stored case and a screen that shouts calls `to_uppercase`
where it draws; screens still own their own labels, which are layout as much as text. An identifier is not a label
(`Essence::ident`), and a design doc names the message rather than quoting the sentence. The server holds no player
prose either: an admin command answers with a typed `AdminReply` and `voice` chooses the words.

**A test asserts a relationship, not a definition — and never a sentence.** An expectation is DERIVED from whatever
defines it: `unlock::mine(Wood)`, not `3`; `b.hardness_ticks()`, not the hold length; the `Msg` variant, not the line it
renders to. So a retune moves the `const` and the suite still passes, a rewording moves `voice::en` and the suite still
passes, and a test that fails is reporting a bug rather than an echo. Watch for the three shapes that look like tests
and are not: one that restates the arithmetic of the function under test (`waste_permille(1) == 1600`), one the types
already guarantee (`assert!(v.len() <= N)` over an `[_; N]`), and one whose name promises more than its body checks —
that last is worse than a tautology, because it advertises a guarantee nobody wrote. Prefer the shape that cannot be
fooled: sweep every row instead of spot-checking, assert an ordering or a round-trip instead of a value, and where a
sweep could pass vacuously, count what it examined and assert the count. Exempt, because the value IS the contract and
nothing in Rust can derive it: wire and save-format goldens, WGSL mirrors, RFC vectors, and the derived-output pins that
catch a curve changing SHAPE (`xp_curve_table_is_pinned`, the taper goldens, `worldgen_golden_hash_pins_sim_version`). A
pin earns its place by naming a bug a derived test would miss.

**Comments carry invariants, not history.** Say what breaks if the reader changes this, and what alternative was
rejected — in a sentence, not a story. `crates/digblock-core/src/stream_budget.rs` is the house style. Do not write: the
bug's biography ("this shipped twice", "it used to be X"), a version changelog, a tombstone for deleted code, or the
same argument twice in a `//!` header and the item below it. Git carries all of that. Cap `//!` headers at ~10 lines and
`///` items at ~6; anything longer belongs in a design doc, and nothing here builds rustdoc anyway.

## Commands

```sh
just run                  # desktop app
just run --drill          # lifecycle drill (suspend/resume + device-loss)
just bench                # deterministic perf run: release, scripted flight, CSV + summary
                          # --no-vsync for uncapped raw-cost percentiles; findings in BENCH.md
just test [FILTER]        # the whole suite, --no-fail-fast: one run lists every golden a
                          # change moved, instead of one per run
just gate                 # check + check-ios + check-web, the three-command habit in one word
just check                # the gate: fmt --check, clippy -D warnings, `just test`
                          # (needs Docker: server e2e boots the apps/master container)
just check-ios            # cargo check for aarch64-apple-ios (no Xcode needed)
just web                  # wasm build + serve at http://localhost:8080 (Chrome/WebGPU; needs
                          # wasm-bindgen-cli at Cargo.lock's exact version)
just web-live             # same, probing the production master, ?overlay&spikelog by default
just check-web            # clippy for wasm32 + check-shaders (needs Chrome)
just check-shaders        # every shader through real Chrome's WGSL front end — the gate naga
                          # cannot be (DIG_CHROME=<path> if Chrome lives somewhere unusual)
just play                 # master container + native server + client at the menu
just play-web             # same but serving the web bundle
just play-docker wifi     # desktop client vs containerized stack (clean | wifi | cellular netem)
just ios-sim / ios-device # build+run via Xcode (full Xcode required)
just master               # .NET master natively (QUIC probe needs `brew install libmsquic`)
just server               # dedicated server (journal in ./data; joining is token-only)
just bot -- --bots 4      # headless bots: pick a server via the master, mint tokens, join
just compose wifi         # dockerized master + server behind tc netem
just compose-bots wifi    # same + a bot soak, exits non-zero on missed snapshots
just profile              # release-speed build with debug symbols
just fmt-md / check-md    # dprint markdown re-wrap and its reporting half
```

Single test: `cargo test -p digblock-core --test worldgen` or `cargo test -p digblock-core <name>`. CI is intentionally
disabled pre-deployment; `just check` + `just check-ios` are the gate. Rust formatting uses hard tabs at
`rustfmt.toml`'s width. **Markdown is hard-wrapped by tool, never by hand** — don't wrap to a column you eyeball, and
don't reflow a paragraph you didn't otherwise touch; the dprint plugin is version-pinned because it decides the
wrapping. Base images and toolchains in CI and Docker are **suite-pinned on purpose** (builder and runtime must share a
glibc); bump `docker/Dockerfile*`, `.github/workflows/*` and `apps/master/Dockerfile` together, with a test release.

## Capture, bench & metrics flags

For `just run`:

- **Scene/pose**: `--seed <f64>`, `--pos x y z`, `--tod <deg>` (freezes the sun; dev builds, or release **with
  `--exit`**), `--draw-dist N`, `--mobile`, `--workers N`, `--pitch`, `--yaw`.
- **Determinism**: `--fixed-dt S`. Required for any A/B — everything animated rides dt, so without it two builds of
  different speed reach `--frames N` in different world states and the diff measures the speed difference.
- **Output**: `--screenshot out.png`, `--frames N`, `--exit`, `--metrics-csv`, `--overlay` (backquote toggles),
  `--no-vsync`, `--offscreen`. `--exit` is fatal when nothing can end the run.
- **Motion**: `--fly N` (fly the bench path then freeze) vs `--warp N` (land on that pose with no motion behind it) —
  the only way to photograph a TEMPORAL artifact. `--walk` swaps the flyover for an eye-height oscillation one block
  forward and back; screen-space artifacts scale with SCREEN motion, and the walk is the regime players report from.
  `--bench-speed B` sets the flyover's blocks/s — **leave the default alone**, every BENCH.md baseline was taken there
  and it is a different STREAMING regime rather than merely a faster one. Always pair `--fly` with `--bench`; `--fly`
  alone silently takes the interactive path and is not hermetic. For FLICKER, diff a `--walk` frame against the one
  exactly one oscillation period later at fixed dt.
- **Attribution**: `DIG_STUB=<terms>` removes one shading term per run — water
  `wvert wsun wssr wripple wrefract wflat spray nowater`, GI `gisun gigather ginee giflat giatrous`, raster
  `noscene noraster noglass`. Uniform-branch gated, so an un-stubbed run pays nothing; a typo is fatal. `noscene` is the
  CLEAN raster stub; `noraster` drops the prepass too and is an UPPER BOUND only. Never subtract the two to "get" the
  prepass. On web the same masks ride `?stub=`. `--max-lights` / `?maxlights=` cap the GI light list (the Light Sources
  slider's lever, unsnapped for sweeps) and `DIG_GI_ITERS=<gather>,<sun>` / `?giiters=` set the resolve's ray budgets —
  A/B levers only, GI-DESIGN.md R36 owns the rules.
- **Scenes**: `DIG_VAULT` (sealed stone vault + torch; `DIG_VAULT_DARK=1` drops the torch and implies it), `DIG_ROOF`,
  `DIG_TUNNEL` (sealed bore past the cascades' coverage — the unproven-daylight capture; pair with a low `--pos`, and
  the same run above `SEA_LEVEL` is the geometry-identical control), `DIG_DIVE`, `DIG_SHAPES`, `DIG_RIVER` (flooded
  basin + `--pitch -35`, the only pose with a SHADED water surface on screen), `DIG_PLAYERBOX=<item>` (a fake remote
  holding a HELD-ITEM WIRE id, so the scene walks the real `held_icon` decode), `DIG_MINING=<level>`, `DIG_TORCHES=<n>`
  / `DIG_TORCHES_FAR=<n>` / `DIG_TORCHES_WALL=1`, `DIG_CONFETTI=1`, `DIG_CRACK=<percent>`, `DIG_SIPHON=<percent>` (rides
  `DIG_ORE`, fatal without it), `DIG_DOORS=1`, `DIG_CAST=<percent>` (poses the cast channel's aura at that progress with
  no hold behind it), `DIG_ORE=<tier 1..=4>` (hollows a chamber around the nearest deposit and grants its level — the
  carve must not touch the vein, or the anti-dupe deletes what is being photographed),
  `DIG_NODE=<heartwood|leystone|springstone>`,
  `DIG_SCREEN=menu|inventory|alchemy|transmute|transmuting|skills|spellbook|hand|bucket|bucket-full|torch|siphon|pick|sigil`
  (the pages are input-only and otherwise invisible to headless capture; the tool names cycle the roster, and screen
  stocking grants every tool since a fresh player owns none).
- **Gotchas**: `--size` sets logical size; the physical framebuffer follows the display's backing scale. **`--display N`
  is mandatory for any frame-time bracket** — refresh rate rides the screen, and a display-paced frame lets the GPU
  downclock (two different `refresh_ms` across a bracket is the tell). Physics ticks ride the wall clock, so cross-build
  stills need `--frames` long enough for both cameras to settle. **A windowed capture must stay VISIBLE on macOS** — a
  covered window gets no surface texture and the run idles on `Occluded` forever; a screensaver counts. `--offscreen` is
  the way out, and is deliberately not implied by `--screenshot` because it also drops the present wait.

Metrics (`digblock-core/src/metrics.rs`) land in the periodic log line, the CSV and the overlay; `DIG_SPIKELOG=1` logs
per-phase breakdowns of over-budget frames. **F7** (or `--gi-drill-at N`) clears the GI's derived state and re-queues
every resident chunk. The overlay's `sky`/`emit` fields ratchet over a session — real terrain entering the window, not a
leak (BENCH.md 2026-08-12); don't re-litigate it. Streaming work is time-budgeted per frame (`STREAM_BUDGET_MS`), so
streaming can't hitch the render loop.

## Architecture

Eight crates, thin launchers, a dedicated server, and a .NET master. The boundaries are load-bearing:

- **`digblock-core`** — pure game logic. No GPU or windowing deps; everything testable headless.
- **`digblock-platform`** — the winit shell around a headless-tested lifecycle state machine. The **only** crate allowed
  to touch `winit` or `wgpu::Surface`. Defines `Game` and `Gpu`.
- **`digblock-client`** — the client: world state, renderer, UI, menus, audio. Renders into `&TextureView`s borrowed per
  `render()` call and **never sees the surface**, so it can't hold a stale swapchain texture across a suspend.
- **`digblock-assets`** — compile-time embedded textures, WGSL and fonts. Shaders are validated **twice, by two
  different front ends**: `cargo test` runs naga (permissive, no GPU); `just check-shaders` hands every entry of
  `shader::ALL` to a headless Chrome, which rejects things naga accepts — the failure is a whole invalidated pipeline in
  the browser only, and that shipped twice. Neither covers pipeline-level binding mismatches
  (`every_pipeline_builds_without_a_gpu` does) or running the page.
- **`digblock-net`** — protocol + client netcode, **sans-IO** and wasm-clean. Postcard, append-only evolution,
  `PROTOCOL_VERSION` bumped on breaks. Holds the predictor, pending-edit tracking, remote interpolation, and a loopback
  transport for plain `cargo test`. **`digblock-net-native`/`-web`** isolate the transports.
- **`digblock-persist`** — journal format, `WorldStore`, `FsWorldStore`. Sits above core AND net.
- **`apps/web-worker`** — a cdylib over `digblock-core` ALONE, exporting one function. Separate from `apps/web` because
  that module's `#[wasm_bindgen(start)]` reaches for `window()` and would panic in a worker, and every worker
  instantiates its own linear memory. **Never give it a `start`, and never let in a dep that pulls `web-sys`.**
- **`apps/server`** — authoritative fixed-tick sim (`sim/`, socket-free, tested by injected events), wtransport layer,
  master client, append-only journal with boot compaction. Terrain is never sent; join sync = seed + edits. Old-version
  journals archive to `.vN.bak` — this destructive path is server-only.
- **`apps/master`** — .NET 10 minimal API, not in the cargo workspace. First-party packages only. Verifies reachability
  with a mandatory QUIC probe pinned to the registered cert hash (no probe-off mode). **Never put `Urls` in
  appsettings.json** — it overrides `ASPNETCORE_URLS` and breaks the container bind. **EF Core turns SQLite's WAL on by
  itself**, and WAL does not work over SMB, which is what the production volume is — `Data/JournalModeInterceptor.cs`
  pins DELETE back unconditionally, and nothing above `Data/` may assume SQLite. The HTTP contract in `Contracts.cs` is
  mirrored by `apps/server/src/master.rs`, `apps/desktop/src/services.rs` and `apps/web/src/services.rs` — **change all
  four together.**

## Networking & dev gotchas

Multiplayer is master-first with **no direct-connect path**, and **playing online needs an account** — Google, Facebook
or Microsoft through the master, never a provider token in the client. Read `AUTH-DESIGN.md` before touching any of it.
Display names are account-owned and globally unique; canonical `validate_name` in `digblock-net/src/protocol.rs` owns
the bounds and charset, mirrored in the master's `Names` class — which is why names stay ASCII-only, and why
`client.name` is trustworthy server-side.

Admin is **accounts, never names** — the name compare it replaced made joining as "admin" a complete server takeover. A
display name resolves to an id ONCE at boot, so a rename doesn't drop admin and an unresolvable admin is fatal rather
than silently granted to nobody. `/reset` is a TEMPORARY testing lever. `/give` hands out ESSENCE rather than blocks on
purpose — a block would be a world edit with no journal entry behind it, while essence rides the standing rebase; it
parses `Essence::ident` — the untranslated operator's word, deliberately NOT the screens' label, so an admin types the
same thing whatever language the client shows. Compass in `/tp`: east = sunrise = −x.

- Use `127.0.0.1`, **never `localhost`** (macOS resolves it to `::1`; the server binds IPv4 only).
- Anything that can't open a browser signs in through `POST /api/dev/signin`, honoured only under
  `ASPNETCORE_ENVIRONMENT=Development` — a master with it set anywhere else **refuses to boot**. The dev master also
  needs `Master__Probe__AllowPrivateTargets` and `Master__Dev__ProbeHostOverride`; the justfile's `DEV_MASTER_ENV` is
  the one copy of all three.
- If a client times out against the docker stack, read the server container's logs before blaming the network — a server
  dead at boot looks identical, and **the server's `conn N: closed` line names why**. It is usually the only side that
  can: a browser's `WebTransportError` carries an empty message when the session itself failed.
- Extra windows: `just run -- --master-url http://127.0.0.1:5200`, plus `--join --dev-signin <subject>`.

## Multiplayer model

Joining is account-authenticated and token-mediated: the client presents its SESSION to the master, gets a one-time
token, sends it in `Hello`, and the server consumes it via `POST /api/tokens/verify` — which answers with the display
name **and the `AccountId`**. Verification fails **closed**; connected players are unaffected by a master outage.

- **The client never asserts who it is.** `Hello` carries opaque token bytes and nothing else about identity. The one
  exception is bounded and temporary: `ClientMsg::ClaimLegacy` (AUTH-DESIGN.md §6).
- **One account holds one live connection** — two would stomp each other's persisted record. Every deliberate exit calls
  `NetClient::disconnect` (`Bye` PLUS a bounded flush: the message alone only reaches a stream buffer, and dropping the
  transport closes before it drains). `Game::on_exit` is that hook, kept apart from `on_suspend` because a backgrounded
  phone may come back; on web the departure arrives as winit's `exiting`, so wiring a goodbye to suspend alone would
  miss closing a tab.
- **A transport without datagrams is a first-class client** — iOS Safari ships `WebTransport.datagrams` with no writable
  side, a missing API rather than a degraded network. Such a client announces `NoDatagrams` right after `Hello` and its
  unreliable traffic rides the reliable stream carrying **the datagram's OWN bytes**, decoded by the same call on both
  ends. The announcement is **made, never inferred**. A wrapped snapshot is still DROPPED on a full queue rather than
  going through `send_frame`, which kills a client whose queue refuses a frame, and it **may not spend the whole queue**
  (`WRAPPED_SNAPSHOT_RESERVE`): wrapped, it leaves at one stream's flow control, putting a LOSSY class where it can fill
  the queue a LOSSLESS one dies on.
- **Journal sync is a push with no acknowledgement**, so nothing notices a column a client lacks — the symptom is a
  chunk that renders as pristine worldgen forever. Three things stand behind it, in order: the client's own
  `EditOverlay`; a sync scan on any draw-distance change, GROWING OR SHRINKING; and `sync::pump_resync`, idle-gated
  because a walking client is already handed what it enters. Replay is LWW so a repeat costs only bytes — the one thing
  it must not do is stomp an in-flight prediction, which is why `JournalColumn` skips cells `pending_at` still owns.
- Movement uses an **adoption envelope**: the server runs the same `Player::tick` and adopts claims within
  `POS_TOLERANCE` that aren't inside geometry, with an EWMA of claim−sim deltas suspending adoption under sustained
  directional bias (the edge-riding exploit). Rejected claims are corrected by the next snapshot via rewind/replay,
  surfaced as a decaying camera offset, never a snap.
- **The camera is presentation-only**; gameplay always reads the authoritative `Player::pos`. It interpolates between
  the last two tick states, and a rewind must carry the lerp anchor with it. `NetEvent::Ack` is dispatched ahead of
  `pump_net`'s edit-budgeted queue: an ack delayed past `BUF_CAP` forces every later snapshot down the replay path.
- Fluid runs only on the server; results broadcast as ordinary edits. Dig/place is predicted locally and validated
  server-side by re-raycasting from the claimed origin.

## Items, tools & the hotbar

The hotbar plus a fixed-size inventory grid are player-authored *assignments* over the count-based inventory: a type
lives in exactly one slot, kept coherent by `reconcile_layout`, run identically on both ends after every count change.
The grid size is a TEST (`the_grid_holds_every_type_a_player_can_own`), and `from_wire` pads so an older save decodes
with its arrangement intact. Layout rides the reliable stream in-order with edits and persists per ACCOUNT.

- `EditRequest.slot` and `DigStart` both declare the hotbar slot used, and Place/Collect must match its item. Honest
  clients can't race it because Layout precedes the edit on the same stream; the declaration exists because the input
  path is jitter-buffered, so without it a player who picks up the siphon and holds on a vein in one motion is refused
  against the hand they had a moment ago. That entry is CLIENT-DECLARED and `from_wire` believes any tool id, so
  ownership is **re-asserted** in `reconcile_layout` against `Inventory::owned_tools` — without it a crafted Layout is
  enough to swing a pick you never made. An empty roster bares the slot, which also auto-equips the first crafted
  vessel.
- **Holding a tool is the tool axis AND the selection.** `Hotbar::tool_in_hand` asks both, answering `Air` whenever the
  bar is not pointing at the tool slot; everything but the bucket used to ask `active_tool` alone. The server keeps the
  selection ON THE BAR, not in a field beside it, since a two-place answer can drift from the client's.
- A slot whose count reaches zero is **released**, not greyed — safe only because the engine reconciles and mirrors the
  layout BEFORE the frame's clicks send edits. Every path that names a tool draws it through `hud::tool_icon`;
  `item_icon` alone is not enough, because `HotbarItem::WaterTool` names the SLOT.
- **The water tool's charge is stored as the inventory's Water count**, capped at one. Storing it there rather than in a
  field of its own is load-bearing: prediction, rejection rollback, `InventoryFull` rebase and both persistence formats
  carry it with no new machinery, and `reconcile_layout` sweeping Water out is also the migration for saves written when
  it was a stack. Empty takes the fluid-aware ray, charged the ordinary one — the pour only works because the ray
  switched, since the fluid ray stops INSIDE a source with no face normal to build against.
- **Your own held item is the viewmodel**, authored in view space with its own narrow projection, drawn in a fifth pass
  that CLEARS the shared depth attachment. Its bob is advanced in `update`, NOT `step_audio`, which early-returns with
  no audio sink and would freeze the hand in exactly the runs that photograph it.

## Blocks, shapes and the grid

Block behaviour is data-driven via the `blocks!` macro — one row produces the `repr(u16)` enum, `COUNT`/`ALL` and the
`PROPS` table together. Adding a block = one row + PNG + `LAYERS` entry. Per-shape geometry, the slab rules and the
physics contracts are documented at the code in `shapes.rs` and `player.rs`; these are the cross-file ones:

- A row states what a dig hands back in exactly one of two ways, never both (`table_sanity`): `drops(x)` is
  unconditional, `crumbles(x)` applies to the WORLD'S copy alone. `dig_drop()` is the pristine answer;
  `dig_credit(edited, alchemy)` is what an actual dig pays.
- The texture layer index is RUNTIME ONLY (mesh vertex bits, never the wire or a save), so the list may be reordered —
  what it may not do is drift from `digblock_core::block::tex`, which mirrors it position for position; a `const` assert
  in `digblock-client/src/texture.rs` fails the BUILD on a skew. **`LOG`/`LOG_TOP` is the tree; the `LUMBER_*` set is
  planed timber.** Derived textures come from `tools/texgen`, outputs committed — a run rewrites EVERY output and the
  noisy recipes share one rng, so adding a `save` rerolls everything after it.
- Chunk cells are `u16` end-to-end: 10-bit id, spare bits, STATE nibble, travelling wire and journal as-is. Mesher,
  physics, fluid and raycast all key off the tables — **no per-block special cases elsewhere.** `state_valid` is THE
  wire gate for state bits, and `Facing` is axis-named (`Dir::West` is +x — compass names are a trap). Vertices pack
  positions in twelfths and shaders decode with an integer whole/remainder split so lattice points stay exact under GPU
  fast-math; **chunk.wgsl and water.wgsl must always agree.**
- **A door is one object in both places that could betray it**: `shapes::object_bounds` boxes the pair from either half
  (the box is taller than one cell — a caller must not assume a box fits inside its cell), and `shapes::tint_from_below`
  anchors the upper half's tint on the lower cell.
- `overlaps_solid`/`placement_blocked` are the shape-aware occupancy contract shared by client place-rejection, the
  server's InsidePlayer scan and the adoption envelope's probe — **standing on a shaped block is NOT "inside solid"**. A
  step and a stair are deliberately the same motion, rising at `MOUNT_RATE`. **Never rate-limit a landing.**
- **The material × shape grid**: `material.rs` is the substance axis and `block::ShapeFamily` the form axis, and **five
  things derive from the pair** rather than being tabled over — unlock level (`max(shape, material)`), craft price,
  craft batch, the earned half of the strength wall, and the transmute tab's families. `craft_units: None` IS the
  recipe-less set; `node_units` is its opposite number, so no material can both cost and pay essence. The BLOCKS shape
  sits at the first level on purpose: progression belongs to the materials (`grid_gates_compose`). Functional items have
  NO material — priced by hand, with `unlock::functional`/`alchemy::functional_ideal` pinned twins.

## Skills & XP

Three skills (Mining, Alchemy, Arcana), designed in `SKILLS-DESIGN.md` and — for Arcana — `MAGIC-DESIGN.md`. Everything
shared lives in `digblock-core/src/skills.rs`:

- The `Skill` enum is `repr(u8)` and **append-only** — the index is a wire/persistence format. Storage is XP-only `u64`
  totals; level is always derived. The XP curve is a **pinned integer table**, never `powf`.
- Dig speed keys on the block's `DigFamily`, and a family declares only its TWO endpoints; one shared `DIG_TAPER` table
  decides how the journey between them is spent, so the SHAPE is authored once and no family can drift. The taper's
  increments **strictly decrease**. `hardness_ticks` is the BASE the curve scales, **not** the duration at any reference
  level: don't assert that `dig_required` at some level returns the raw hardness, and don't hardcode a hold length in a
  test — derive it.
- **Letting go of a hold decays it, it does not reset it.** Client prediction, the server's `DigState`, the completion
  gate and offline play all call ONE function, `dig_decayed`; both ends store the banked PEAK plus an idle count and
  never decrement in place, which is what stops two rounding chains drifting apart. Every seam errs in the server's
  favour, so an honest completion is never short.

**Every level threshold in the game lives in `digblock-core/src/unlock.rs`** — nothing else may hold an unlock number,
and no caller compares levels itself, including the step ones a match arm used to hide (`PICK_PLANE_AT`,
`PROSPECT_FAINT_AT`, `MASTERY_AT`, `BENCH_FASTER_AT`, `WASTE_DIP_AT`). Retuning one is that line plus whatever
deliberate pins fire — `unlock/tests.rs`, and the faucet golden in `alchemy/tests.rs` for a Mining level — and one
`just test` names all of them at once. That claim is checkable, and worth re-checking after any change here: bump a
threshold, run the suite, and every test that fails should be a pin. One that is not is a copy somebody wrote down.
Nothing else should move: the character screen's teasers READ their levels. `craft` and `pickup` are DERIVED, never
tabled. Two dig gates, one pattern: `mine` is the strength wall keyed on the `DigFamily`, so every block of a family
opens together; `mine_material` is a SECOND gate, and `mine` takes the later of the two, so a material can only push a
block later, never earlier. `pickup` refuses a **craft-only** block below its own craft level, keying on the DROP and on
craft-*only*, never craftability — `Wood` has a recipe and grows on trees. Everything asks `dig_blocked`, which returns
the first unmet `Requirement` — the block, the skill and the level. No wording lives in core: the client's `voice` turns
one into the ONE sentence that gate speaks, naming the block and the level that opens it, which is why the wire sends a
reason and never a sentence.

**XP is server-computed at exactly three chokepoints** — the dig-credit block in `sim/edits.rs`, `process_alchemy` and
`process_cast` — and reaches the client as `ServerMsg::XpGain` carrying the new **TOTAL** (idempotent, reorder-safe).
Join sync always sends `ServerMsg::Skills`, even empty, right after `Layout`. Offline play awards through the same core
functions in the engine's offline arms only — **never award in the shared prediction path, or multiplayer double-pays.**

## Alchemy & the economy

**Essence is found, not ground** (SKILLS-DESIGN §14): bench extraction is GONE, and the siphon's field draw over the
node layer is the currency's only faucet. The economy lives in `digblock-core/src/alchemy.rs`, all integer math. Six
load-bearing rules:

1. **Nothing the world hands you may be transmuted.** `unlock::craft` and `alchemy::craft_ideal` both open on
   `BlockType::is_bench_only` — DERIVED, never listed.
2. **The Rubble rule** — Stone/Sandstone `crumbles: Some(Rubble)`, and Rubble is the mason's feedstock. `crumbles` and
   not `drops` because a stone somebody PLACED hands itself back, keyed on whether the cell has ever been written.
   **Read the cell's edited bit BEFORE the dig's own write on both ends**, or every dig finds itself standing in an
   edited cell.
3. **No craft creates essence** (`no_craft_creates_essence`): one faucet, one drain, in different modules.
4. **Crafts are feedstock + catalyst** — one cube of the material's feedstock plus its catalyst (never prorated by the
   shape), yielding `shapes::craft_batch` pieces, so matter conservation is structural. Metal and Crystal are the
   essence-only exceptions: transmutation proper, for substances nothing in the world is made of.
5. **The essence indices ARE the migration.** `Essence` keeps the retired Material enum's numbering. **Never renumber;
   renaming is free** — the indices are a wire and persistence format, the identifiers are a format nowhere.
6. **A price must be payable at its own level**, and its sharpest corollary is pinned separately: **the essence tool
   costs no essence, ever** (`the_essence_tool_costs_no_essence`), or the currency could never bootstrap.

The alchemy screen (K) is ONE page, and **the bench works with the screen closed** — `Engine::update` ticks
`AlchScreen::tick` whenever the screen is open OR `busy()`, and only leaving the WORLD abandons. **The page shows the
unlocked catalog plus ONE teaser tier**; tiles pack left but keep their true indices, so hiding never shifts hit
identity. **The button QUEUES a count rather than buying it**: crafting drips one `Craft{count:1}` per
`craft_pace_ticks`, and the progress row is also its CANCEL.

Wire facts: essences ride every format as flagged ids. `AlchemyOp::Convert` is a RETIRED verb kept decodable and refused
with the standing rebase. `ClientMsg::Alchemy` is NOT client-predicted — the server answers every op with an
`InventoryFull` rebase — and rides the edit action queue so craft-then-place in one tick debits in wire order. The bench
token bucket lives in op validation, NOT `apply_alchemy`, so the offline economy is identical and unpaced. **A craft
that would vanish into a type's cap is refused before any debit** — `Inventory::headroom` is both the refusal and what
greys the button, so the offer and the rule cannot drift.

## Nodes & ore

**Derived, never stored — and every essence source is a NODE.** `worldgen/nodes.rs` is the unified layer, asked with a
`Levels` pair because the seven kinds split across two reveal axes: four ore tiers on Mining, three surface nodes on
Alchemy (Heartwood in pristine trunks, Leystone in the deposit stream, Springstone in fresh-water sand beds). Heartwood
is the day-one node, because the economy has to be able to begin. A deposit is a salted 3D position hash, so
`build_column` returns the bytes it always returned, the goldens don't move, and nodes appear retroactively in existing
worlds. Depth bands have FLOORS as well as ceilings, so depth predicts tier. **The one stored node is a REGROWN one**:
`worldjob::regrow` rolls every spent face standing where its find was, once a game day, and writes the live face itself
back — the edited bit is monotone, so no write of the host could ever reveal again. Safe because no live face is an
item, so only the sweep can put one in a cell; `nodes::revealed` finds a stored face at any level, since every client's
mesh already shows it. The odds scale with `presence::RollingPeak` — the day's peak head-count, fed by each host arm.

- **The unedited rule is the anti-dupe.** A node manifests only in its pristine HOST; without it, placing the host back
  into a spent cell would regrow the find. `JournalIndex::record` only ever inserts, so "touched" is monotone. The
  client needs the same answer, which is why `Pipeline` keeps its `EditOverlay` in MULTIPLAYER too — but there it
  MIRRORS the journal rather than the client's guesses. **Recording predictions instead is unfixable**: a rejection
  reverts through the same `set_cell` and `record` is insert-only, so the cell stays edited on the client and pristine
  on the server, forever.
- **The reveal stamps block IDS into `MeshInput`, in the worker** — not texture layers, so the mesher, the texture path,
  the emissive bit and shaped culling all see a real node block. Pass two touches only the CLAIMED indices, never a
  sweep, or a placed spent rock turns to stone for every visitor below its tier.
- **One question answers every dig**: `nodes::dig_mode(...)`, asked identically by client prediction and the server. A
  draw is timed as the node, writes the kind's own spent face rather than air, pays both skills, and shows no crack.
- **A node is world geometry and never an item.** `is_slottable` is false for every live node — both the rule and the
  migration for saves written while a siphon handed ore over. The essence is deliberately NOT predicted (one `inv_delta`
  per `PendingEdit`, so the draw takes the standing rebase); the CELL is, so the find turns spent under the hand.
- **A draw ENDS the press that made it** (`Engine::drawn_cell`); the latch names ONE cell and is cleared only by the
  button coming up. It has its own effect and its own two sounds, because a crack would say the rock is about to fall.
- `gi::SHAPE_CLASS_BASE` splits the GI class byte between full-cube ids and shaped covers; a shaped MATERIAL costs a
  dozen-odd covers, and the pin reports the remaining headroom. **Watch it when adding a shaped material.**

## Tools & the Resonant Pick

Four tools, every one crafted: the two vessels (bucket and **Essence Siphon**, level-1 crafts priced in felled timber
ALONE), the **Resonant Pick**, and the **Master's Sigil** (the last thing either ladder unlocks, priced entirely in
Aether, and it DOES nothing — the point is that it is the catalog's last line). Ownership is the count; a fresh player
owns none, and `Hotbar::active_tool` is an `Option`.

The pick breaks the PLANE of the face it hits, derived on both ends from the ray (`pick::cells`) so the client names one
cell and the server decides what the swing touched. Two rules: every extra cell must yield the **same credit**
(`begin_multi` carries ONE `inv_delta`); and the hold multiplier is **level-only** — a world-derived one would let the
two ends disagree by the whole multiplier over a single differing cell, far outside `dig_grace`. **The reach is a bonus
in material, not in XP**: cells behind the aimed one pay `PICK_EXTRA_PERMILLE`, capped at the aimed cell's own award.

## Lighting (RTGI)

Probe-traced GI, the **only** render path — no flag. The raster stack it replaced is deleted; with AO gone the mesher's
quad winding is fixed rather than AO-keyed, and the vertex words are repacked.

**`GI-DESIGN.md` is the reference** — data model, pass order, every tuned constant, R13–R35 with their measurements, a
"What is NOT built" list, and **`## Invariants`, the list that bites if broken.** Read both before changing anything
here; several plausible ideas in there were built, measured and **declined**, with the numbers recorded.

Four reach outside lighting, so they can catch you when you weren't editing lighting at all:

- **Water writes OPAQUE**; alpha is the falling-sheet streak gate and sits at exactly 1.0 on every still surface.
- **The see-through predicate is `MeshBucket::Translucent`, never a render class** (R35): it picks the blended glass
  stream, opens the GI's tinted transmission, spares the turf under a pane (`worldjob/grass`), decides which face water
  emits toward, keeps a shaped run culling its own interior, and leaves the water surface unpinned under a pane — so a
  new translucent block is a bucket, not a class audit. Every one of those sites keyed on the CLASS once and each grew
  its own visible hole; `a_translucent_cell_never_covers_whatever_its_shape` is the sweep that generalizes it.
- **Every node row is `seams(level)` and nothing else is** — buried rock must light nothing.
- **`gi::carried_light(block, mining)`** is the one place an emitter answers to its HOLDER: it raises the pick's level
  and whitens its cast tint at the rank `unlock::pick_cells` widens the swing at, riding the light entry itself, because
  two players sharing a LUT row must be free to cast differently. A remote's tool casts at its row's level — their
  skills are not in the snapshot.

## Worldgen & the chunk pipeline

Everything is a pure function of `(x, z, seed)` — column content never depends on generation order. Structures are a
decoration pass using pure apron evaluation, so every chunk within `STRUCTURE_APRON` computes identical structures
independently and they span borders with no pipeline state; trunk roots are the single exception to decoration's
write-into-Air-only rule. Sky chunks resolve to the shared `AIR_CHUNK` sentinel Arc — never meshed, **recognized by
POINTER**, which is why `job_wire`'s air flag exists.

The main thread owns the chunk store and scheduling; workers run pure functions over immutable snapshots — a mesh job
gets a padded copy with a one-block shell from every neighbour. Staleness rides generation counters: results whose
`generation` doesn't match are dropped. Edits go through `set_block`/`set_cell` (copy-on-write, bumping only neighbours
whose shell contains the edit).

**Water surface knitting** is the subtle part. `mesher/water.rs::corner_drop` is a pure function of the horizontal
lattice CORNER — never of the cell emitting the face — which is what lets two cells sharing a corner knit without a
crack, chunk borders included. Its pins, in order: water above or a covered cell → full; an air cell with water below →
plunge, UNLESS a resting source touches the corner; else the complement of the **strongest** touching amount. Strongest,
not the mean: the sim halves the amount on each spread, so averaging drags every corner toward the thinner water beyond
it. `water.wgsl` maps that code to a height **linearly** — the two halves are one invariant, so any change to how
corners are chosen must be checked against the mapping and not just the codes.

## Scheduled world jobs

`digblock-core/src/worldjob.rs` is the seam and carries four rules: `grass` (creep onto sunlit dirt, smother under
covers), `leaves` (foliage rots once nothing anchors it), `wisplight` (conjured light fades — the cadence IS the spell's
whole lifetime, so a quit-and-reload can never leave a permanent free lamp) and `regrow` (a spent node may come back, at
odds that climb with the day's peak head-count). A `WorldJob` runs on a clock over blocks of a given type near active
players, and every `apply` gets a `Run` — the slot AND the day's peak players — snapshotted when the sweep starts.
**This is not fluid and must not be built on it** — fluid is a continuous per-tick sim over cells in flight, this is a
rare batch sweep. They share only their write path.

- **The clock is the GAME's day** (`sun::DAY_SECS`), fixed by `Schedule` rather than chosen per job — a server's local
  09:00 means nothing to a player elsewhere, and the game clock is already dictated to every client.
- **The anti-dupe is enforced, not documented.** A journal write sets the cell's monotone edited bit and
  `nodes::revealed` reads it as spent, so a sweep over a node host would destroy every reveal in a band around every
  player. `Scheduler::register` refuses such a scope — live faces included, since a stored one is a node outright — and
  `a_node_host_is_never_sweepable` walks every block to keep `worldjob::node_host` and `revealed` naming one set.
  **Watch it when a new block becomes a node host.**
- Core is IO-free: `tick` reads through `CellSource` and RETURNS writes, so each side applies them through its own
  authoritative path — the server's `apply_write` (journal, protection, LWW, broadcast) and the engine's
  `Pipeline::set_cell` offline. Both arms run the same scheduler, or a rule forks.
- Catch-up is capped at one run (`last_run` advances when a sweep STARTS); one sweep at a time; the column set is
  snapshotted at the start, because players move and a shifting set gives uneven coverage.
- **Leaf decay's frontier candidates are its propagation engine**: a felled trunk's edits expose only the adjacent ring
  of leaves, and each rotted ring's own writes recruit the next — decay applied anywhere but the edit path stops
  canopies melting. A leaf is anchored by trunk blocks INCLUDING the heartwood faces (a siphon must not orphan the
  canopy), or by foliage resting on ground — which is what spares bushes, so "a leaf implies wood nearby" is a trap.
  `every_pristine_structure_leaf_survives` walks real worldgen to keep the walk cap ahead of the canopies decoration
  grows — **watch it when widening a canopy.**

## Spawn & protected cells

Both modules carry thorough `//!` docs. What those don't say, and what bites:

- **The `Edits` view is edits only, never a chunk store.** A store answers `None` for non-resident cells and the support
  test reads `None` as "trust worldgen", so wiring one in makes unloaded columns read as void. The server's journal is
  the correct and complete source, fluid included.
- **Protection is always on, and a `protect::Region` is a set, not a box.** Every arrival pad — spawn and each resolved
  waystone — locks a `spawn::LOCK_RADIUS` sphere, so `immutable` takes a slice and `/lock-spawn` is gone. Offline the
  engine enforces its own spawn sphere; a SERVER's regions are its own secret and reach the client only as refusals.
- **`spawn::lock_region` CONTAINS every cell the search inspects** — containment, not equivalence, since the sphere also
  freezes unvalidated cells around and beneath the pad. That containment is why a locked spawn is permanently valid.
  `/set-spawn` validates with the same predicate and deliberately skips `SPAWN_FLOOR` — an operator aiming at a block
  has made a choice.
- **A player-authored action asks `requester_regions`, the sim's own writers ask `protected_regions`.** Admins are
  exempt from the first — the operator who moves spawn must be able to repair it — and never from the second, or an
  admin merely being ONLINE would let water eat the spawn pad.
- **Protection refuses the fluid *write*, not the schedule** — that is what stops water sourced outside a locked region
  flowing in. A protected hold is refused at `DigStart` rather than at the completing edit.
- **Rejoin resumes in place**: `PlayerRecord` carries the last eye position *and* the return-to-spawn tick, since
  position alone let a rejoin clear the cooldown.

## UI, text & audio

`ui/theme.rs` owns every spacing token and colour the screens share; a constant only one screen uses does not belong
there. Three load-bearing rules, the last two pinned by `screens::every_screen_draws_text_at_one_size` and
`every_screen_dims_and_panels_the_same_way` because both have regressed visibly before:

1. **Every token is a logical size**, multiplied by `Style::scale` exactly once at the point of use.
2. **A frame has exactly one text size**, with `scale::FIXED_GLYPH_PX` the only sanctioned exception.
3. **A screen is one panel colour over one dim**, menu included.

`theme::fit_scale` steps the WHOLE style down in fixed increments, re-measuring per rung — never a fractional glyph.
`ui_scale` quantizes **only** its derived half; the user's preference multiplies in exactly, so the effective scale is
NOT on that quantized grid — don't write a test that assumes it is.

While ANY UI surface is open the engine synthesizes cursor + left-button from the primary touch at the top of
`Engine::input` — one site gives iOS every screen, and no screen knows about touch. The MENU is the exception:
`a_touch_page_never_names_a_key` and `a_touch_page_can_always_be_backed_out_of` are swept rather than spot-checked,
because what bites is the page nobody thought of. `Menu::can_go_back` is derived from the same match `handle_key` runs.

**Text** — two embedded TTFs rasterized at runtime with `fontdue`, at the exact pixel size they're drawn at, blitted 1:1
on whole-pixel origins; that 1:1-ness is why the Linear sampler costs nothing. Sizes are a **layout box height**, never
an em, and a text `y` is the box's TOP, which is what keeps every `(rect.h - glyph) / 2.0` centring site working.
`Face::run` is the single advance walk behind `measure`, `wrap` and `push_text` — **never compute a width from a
character count.** The atlas's white block is the UV of *every* untextured overlay quad and only ever repacks in
`begin_frame`. This half is glyphs and boxes — what the game SAYS is `voice`, one module over (the third writing rule).

**Audio** — the engine emits `AudioCmd`s into an injected `Box<dyn AudioSink>` (a `None` sink covers
bench/screenshot/drill/iOS). Cadence emitters pace by DISTANCE, not time, so a slow walk still sounds right; strokes
fire only when in water and off the bottom, exactly where footsteps stop. Music is never embedded — the master serves
CC0 tracks and both fillers fetch at runtime.

## Settings & local worlds

**The two resolution knobs**: the world renders into `scene_view`/`post_view` at `surface × render_scale` and ONE
upscale pass takes it to the surface, after which the overlay draws at native — so UI, text, name tags and the crosshair
are never resampled. (Name tags project `view_proj → NDC × size`; the `size` fed to the overlay must stay the SURFACE
size, which is the one way this silently regresses.) GI runs at `render_size / gi_scale_div`, an INTEGER divisor — **a
fraction of the RENDER size, not the surface.**

Which knob to spend is **measured, not intuited** (BENCH.md 2026-08-12): a notch of each buys comparable frame time, but
the GI notch costs far less picture, because the à-trous filter and the upsample already reconstruct that signal. So the
high-DPI default is native resolution with lighting detail stepped down (`density_defaults`), applied at the first
`set_scale_factor` and ONLY when no flag was passed and nothing was saved. `gfx_defaults_auto` is false on every capture
path, so no baseline moves.

Two colour targets are mandatory: water samples the scene it blends over, so its destination cannot be its source.

Persistence is a launcher-injected `SettingsStore`; precedence is CLI/query > saved > platform default, and
non-interactive runs skip load AND save so captures stay hermetic. `Settings` is serde-free; each launcher owns a
mirror. Named local worlds live in `crates/digblock-persist`: the journal (shared `VERSION` — a worldgen retune stales
server volumes and client saves together; the one exception is a provably ADDITIVE decoration change, which rides a
`PROTOCOL_VERSION` bump with saves kept — `landmark_fallback_only_adds` is the shape of the proof) plus the `WorldStore`
trait. Web uses localStorage **not IndexedDB**, because `flush` must run inside synchronous `on_suspend`. Client saves
are **refused, never archived** on a version mismatch.

`Pipeline`'s `EditOverlay` re-applies cells before air classification, so edits survive hard eviction even in unsaved
worlds — **in multiplayer too**, where it mirrors the server's journal. That half is not a nicety: a re-entered column
has no other way back, since `apply_authoritative` drops a cell from `deferred_edits` the moment it lands and the server
prunes its record on a wider radius than the client's residency. What stays local-only is `record_local` — recording in
multiplayer would be recording a guess.

## Browser client

Chrome via wasm32-unknown-unknown + WebGPU. Query params mirror desktop flags
(`?seed= ?tod= ?draw= ?ssr=0 ?overlay ?mute ?master= ?join&server= ?workers= ?frames= ?nodatagrams ?mobile ?uiscale=
?maxlights= ?stub= ?torches= ?torchesfar= ?giiters=`);
defaults are full desktop quality, and a coarse-pointer device boots `EngineConfig::web_mobile` instead — shorter draw
distance with the slider's lower rungs open (`settings::DRAW_FLOOR`) — with saved settings and URL params still winning.

- The loading overlay is a fixed contract with the platform layer (`digblock-gpu-ready`, `digblock-first-frame`,
  `digblock-fatal`), and **it must be PAINTED before wasm instantiation begins** — the double-rAF in index.html is what
  guarantees that on a cold mobile load. Every stage behind it can HANG rather than fail, so a watchdog names the stage
  that stopped, and it reports the page's `navigation` type, because iOS discards a page this heavy under memory
  pressure and a stuck cold load is otherwise indistinguishable from a slow one.
- **A fresh canvas reports 0×0**, which is why every async GPU init ends at `App::adopt_window_size`. winit fills the
  size from a `ResizeObserver` that first fires at the next rendering step, so an init that beats it configures the
  surface at the `max(1)` floor — and the `Resized` that follows lands while the app is still `WaitingForGpu`, where it
  is dropped. winit only sends one when the size CHANGES.
- **Reclaiming iOS Safari's toolbar strip needs a real touch scroll of the DOCUMENT** — script cannot ask, and an inner
  scroll container does not work; the canvas's `touch-action: none` is what makes the handle safe.
- **Chunk jobs run in Web Workers.** Workers can't share linear memory, so jobs cross as bytes via
  `digblock_core::job_wire`. The pool is best-effort: until a worker reports ready — or forever, if the browser refuses
  to spawn any — jobs run inline. `JobDispatch::reset` on `take_dispatch` stops a torn-down world's in-flight column
  landing as terrain in the next one. **`adaptive_effective` must NOT be gated on the executor** — its budget is one
  deadline over jobs, drain AND uploads.
- **Every step of the WebTransport driver names itself**, because browsers ship WebTransport in PARTS: `prop` reaches
  each hop by name rather than a typed getter (reading `.writable` off an absent `datagrams` THROWS, and a throw inside
  the driver task lands in no `Result` — it unwinds out of wasm as an unhandled rejection and the session just expires).
  **The SESSION's account of an ending beats a stream's**, and must be waited for: a connection going down rejects the
  pending read AND settles `closed`, and the read always wins. `stream_failed` parks a stream's reason for
  `SESSION_GRACE_MS` and reports it only if the session stays up.
- **`Device::push_error_scope` is unusable in the browser.** wgpu reads a popped scope through a `JsOption`, which
  counts only `undefined` as absent, while a CLEAN `popErrorScope` resolves `null` — which then reaches a converter that
  panics on anything but a validation or out-of-memory error, so every pop aborts the module whether or not anything
  went wrong. `pipeline_builder::watched` drives the two scopes off the raw `GPUDevice` instead; nothing reachable from
  a browser may go back to wgpu's API.
- `web_time::Instant` workspace-wide; the depth "copy" is a blit into R32Float (WebGPU forbids depth32float as a copy
  destination).
- **Deploying**: `just build-web` is the bundle and `deploy-web.yml` runs that recipe rather than mirroring it. Glue JS
  and wasm are ONE artifact (wasm-bindgen mangles import names with a per-build hash), so a browser pairing fresh wasm
  with stale glue dies at load with `LinkError`. **Cache headers cannot fix this** and twice appeared to — the CDN
  rewrites Cache-Control for `.js` and not `.wasm`. The fix is naming: both files carry the same content stamp, only the
  always-revalidated `index.html` knows it, and a stale index can therefore only 404, never link half a live pair.

## Lifecycle & iOS

Suspend/resume and device-loss are standing regression targets for a future Android port. `Renderer::new` is a pure
constructor so device-loss recovery is "build a new one and re-upload meshes" (`Game::rebuild_gpu_resources`).
`Game::on_suspend` must be cheap and synchronous — the process may be killed with no further callbacks. Test via
`just run --drill` or F9/F10 at runtime; F8 screenshots.

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` — edit the yml and rerun `xcodegen`, never the
project file. Simulator/device builds need full Xcode; `just check-ios` is the fast gate that doesn't.
