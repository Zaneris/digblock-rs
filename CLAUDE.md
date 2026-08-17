# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Rust + wgpu rewrite of the DigBlock voxel engine (originally Java/libGDX, 2015–2017). The faithful-port goal is
complete; the engine is free to diverge and improve. The old Java repo at `../DigBlock` is a behavior reference, not a
constraint.

**Where the detail lives.** This file is orientation plus the invariants that bite if broken. The reasoning, the
measurements, and the alternatives that were tried and declined live in the design docs — read the relevant one before
re-litigating a decision or rebuilding something:

- `GI-DESIGN.md` — lighting: data model, pass order, tuned constants, release notes R8–R25, and an honest "What is NOT
  built" list.
- `BENCH.md` — every perf measurement, including the ones that refuted a hypothesis.
- `SKILLS-DESIGN.md` — skills, XP curve, essences, transmutation catalog, milestones; §13 is the 1–60 content
  compression (the ladders' current shape, the tail policy, the pacing workstream, and the planned content waves).
- `DELVE-DESIGN.md` — design-only: underground dungeons whose fabric heals through ordinary journaled edits (never
  un-edits — the ore anti-dupe's monotone edited-bit survives untouched), deterministic telegraphed hazards, the
  attunement verb, health/death, the Delving skill, and the governed Aether faucet.
- `SOCIAL-DESIGN.md` — design-only: hiscores via the master, mastery held-items in the sigil's mold, player claims on
  the protect.rs predicate, and why trade waits for skill specialization.
- `AUTH-DESIGN.md` — accounts, sign-in, and the master's trust model: the identity model, the OAuth flows, the
  registration algorithm and why the probe order is what it is, the pre-account character migration (§6, including what
  the claim key is worth to a hostile operator), and an honest "what is NOT built" list.
- `NET-AUDIT.md` / `NET-AUDIT-2.md`, `PERF-AUDIT.md` — netcode and frame-pacing audits. NET-AUDIT-2 §1 was the
  requirements doc for `AUTH-DESIGN.md`; all seven findings are closed there.

## Never write a value into prose

**A number lives in exactly one place: the code that defines it.** Comments, doc comments, and every Markdown file in
this repo — this one included — name constants and describe relationships; they never restate what a constant equals. A
retune should touch the `const` and nothing else. It used to touch dozens of files, because the value had been copied
into the prose beside it, into the module's `//!` header, into CLAUDE.md and into a design doc, and each copy silently
went stale on its own schedule.

This is a **maintenance-cost** rule, not a correctness one. Restated values cost nothing the day they are written and
get more expensive every month: the number of copies grows with the repo, so the price of a one-character retune grows
with it too, until changing a threshold means an afternoon of prose edits and a diff nobody can review. Time spent
re-typing a value into prose that already sits three lines above in code is time spent for free.

What that means in practice:

- **Name it, don't quote it.** "clamped to `POS_TOLERANCE`", never "clamped to `POS_TOLERANCE` (0.35)". The reader who
  needs the number has the symbol to jump to; the reader who needs the concept was never served by the digits.
- **Describe relationships, not arithmetic.** "`MOUNT_RATE` is exactly what a 45° ramp gives a walk" survives a retune;
  "`MOUNT_RATE` (= `WALK_SPEED` = 4.3)" does not. Prefer the invariant that makes the value *right* — that is the part
  worth writing down, and it is usually the part a bare number was standing in for.
- **Consequences are values too.** A jump apex, a wade depth, a percentage speedup, a "roughly two rooms apart" cashed
  out in blocks — all derived from constants, all stale the moment one moves. Say what the behavior *is*, not what it
  measures.
- **Formats describe structure, not offsets.** Field order and widths belong to the packing code and the tests that pin
  it; a comment says which fields a word carries and in what order, not the bit indices. Same for wire layouts and array
  strides.
- **Worked examples go in tests, not comments.** A trace through concrete numbers is a test the compiler checks, not a
  paragraph that rots. If a comment is walking an example to prove an algorithm, that example wants to be an assertion.
- **Prose in test bodies names the scenario, not its literals.** The literals are on the next line and the comment
  cannot drift from them if it declines to repeat them.
- **Measurements belong in the bench log, not beside the code.** "this pass costs 1.68 ms, 23% of the frame" is true of
  one build on one machine, and the comment has no way to know when it stopped being true — the code around it changes
  and the figure does not. Say what the shape of the cost is ("the dominant term in this pass", "cheap enough to run
  unconditionally") and let `BENCH.md` hold the dated number, where the build and the machine are recorded with it.

The rule is about *values*, not about digits. These stay, because they are identifiers or inputs rather than documented
quantities:

- **References and versions** — document and section references (`SKILLS-DESIGN §5.1`, the `R8`–`R28` release notes),
  format versions (`PROTOCOL_VERSION`, SIM and journal revisions, `inventories.bin` generations), dated entries in an
  audit or bench log. A version identifies a contract; it does not measure anything, and it changes only when the
  contract does.
- **Names that contain a number** — `Latin-1`, `wasm32`, `.NET 10`, `rgba32f`, `R8Uint`, a stride-2 pass. The digits are
  spelling, not measurement.
- **Runnable commands and their arguments** — `just bot -- --bots 4`, `DIG_TORCHES=8 DIG_SCREEN=torch`, `--pitch -35`,
  `127.0.0.1`. A recipe stripped of its arguments stops being a recipe, and nothing in the code defines these; they are
  what you *type*, not a record of what something equals.
- **A trailing annotation that decodes its own literal**, on the definition line:
  `const SUN_CONE_COS: f32 = 0.99996; //
  cos(0.5°)`. This is not a second copy — it is the one site, made readable,
  and it moves with the value because it sits on the same line. The rule forbids a value *restated somewhere else*, not
  a note explaining an opaque literal where it is defined.

The test to apply: **would this number have to change if someone retuned a constant?** If yes, it is a copy and it comes
out. If it would only change when you deliberately rewrite the sentence around it, it is a name or an input and it
stays.

Tuned values that genuinely need discussion — why this threshold and not a larger one — get discussed in the design doc
**by name**, with the reasoning and the alternatives; the doc is a home for the argument, never a second copy of the
number.

**A design doc may quote a value when the value IS the argument.** "The pick costs more Aether than anything else in the
catalog" is a claim about a price; strip the price and the claim can no longer be checked, and the paragraph stops being
a reason and becomes an assertion. Same for a rejected alternative whose arithmetic is the whole case against it, and
for a target a later note is trying to beat. These are not restatements — nobody edits them when a constant is retuned,
because the argument was made against the numbers as they stood, and a retune that falsified one would need the
*paragraph* rewritten, not a figure swapped. Two conditions keep the carve-out narrow: the code stays the authority
(quote to make a case, never as the place to look the value up), and the quoted figure must be doing work in a sentence
of reasoning. A level table, a tuning list, a "current value" aside — those are copies wearing a doc's clothes, and they
come out.

**`BENCH.md` and the audit logs are the exception, and deliberately so.** A dated measurement is not a restated
constant: it was true of one build on one machine on one day, it is superseded by a later entry rather than edited, and
no retune anywhere can make it wrong. So those files **keep their figures** — frame times, percentiles, delivery rates,
image diffs — because a baseline whose numbers have been replaced by "it got faster" cannot be compared against, which
is the entire reason a baseline exists. What still comes out of them is what comes out everywhere else: a **current**
constant or shipped default quoted inline (`STREAM_BUDGET_MS = 3`, `POS_TOLERANCE = 0.75`, "the default stays
`(n-2).clamp(2, 4)`"), which rots exactly like any other copy. Name the constant, keep the observation. This is also why
a measurement does not belong in a code comment: beside the code it has no date, no machine and no build, and nothing to
supersede it.

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
just web-live             # same, but probes the production master first and defaults the URL
                          # to ?overlay&spikelog (frame-pacing diagnostics)
just check-web            # clippy for wasm32-unknown-unknown + check-shaders (needs Chrome)
just check-shaders        # every shader through real Chrome's WGSL front end — the gate naga
                          # cannot be (DIG_CHROME=<path> if Chrome lives somewhere unusual)
just play                 # master container + native server + client at the menu
just play-web             # same but serving the web bundle; open the printed URL
just play-docker wifi     # desktop client vs containerized stack (clean | wifi | cellular netem)
just ios-sim / ios-device # build+run via Xcode (full Xcode required)
just master               # .NET master natively (QUIC probe needs `brew install libmsquic`)
just server               # dedicated server (journal in ./data; --master-url defaults to the
                          # production master https://master.digblock.net; joining is token-only)
just bot -- --bots 4      # headless bots: pick a server via the master, mint tokens, join
just compose wifi         # dockerized master + server behind tc netem
just compose-bots wifi    # same + a bot soak, exits non-zero on missed snapshots
just profile              # release-speed build with debug symbols
```

Run a single test: `cargo test -p digblock-core --test worldgen` or `cargo test -p digblock-core <test_name>`. CI is
intentionally disabled pre-deployment (`.github/workflows/ci.yml.disabled`); `just check` + `just check-ios` are the
gate.

Formatting uses hard tabs at the width `rustfmt.toml` sets. Markdown is **hard-wrapped, by tool rather than by hand** —
`just fmt-md` (dprint, `cargo install dprint --locked`) re-wraps every doc to the width `dprint.json` sets, and
`just
check-md` is the reporting half that `just check` runs. Wrapping is what keeps editing a sentence to a one-line
diff instead of reflowing a paragraph; don't hand-wrap to a column budget you eyeball, and don't reflow a paragraph you
didn't otherwise touch. The markdown plugin is **version-pinned** in `dprint.json` for the reason the Docker tags are:
the plugin decides the wrapping, so one that drifts rewrites all twelve docs at once.

Base images/toolchains in CI and Docker files are **suite-pinned on purpose** (builder and runtime must share a glibc;
floating `1-slim` once broke a release). Bump the pins in `docker/Dockerfile*`, `.github/workflows/*`, and
`apps/master/Dockerfile` deliberately — together where the comments say so — with a test release.

### Networking & dev gotchas

Multiplayer is master-first with **no direct-connect path**: the .NET master (`apps/master`) owns accounts, lists
servers, distributes cert pins, and mints one-time join tokens; a game server refuses any Hello it can't verify with the
master. **Playing online needs an account** — Google or Facebook through the master, never a provider token in the
client. Read `AUTH-DESIGN.md` before touching any of it.

Display names are account-owned and globally unique, chosen once at sign-in and changeable on a cooldown (a
length-bounded run of `a-z A-Z 0-9 - _`; canonical `validate_name` in `digblock-net/src/protocol.rs` owns both the
bounds and the charset, mirrored in the master's `Names` class — which is why names stay ASCII-only). A joiner cannot
pick one per request, which is what makes `client.name` trustworthy server-side. Chat: T/Enter to type; the server
sanitizes to the Latin free-text alphabet (`is_text_char`/`text_charset`, printable ASCII plus Latin-1 and Latin
Extended-A so European players can type their own words) and rate-limits (`CHAT_*`).

Admin: `--admin <account-id>` (repeatable) / `DIGBLOCK_ADMIN_IDS` grants chat commands
(`/help
/tp /goto /bring /give /time /ban /ban-list /unban /set-spawn /set-spawn-dir /lock-spawn`). **Accounts, never
names** — the name compare it replaced made joining as "admin" a complete server takeover. A display name also works and
is resolved to an id ONCE at boot, so a rename doesn't drop admin and an unresolvable admin is fatal rather than
silently granted to nobody. Players read their id from the in-game Account screen (Copy ID). Bans record the account AND
the IP in `data/bans.json`, spawn overrides in `data/spawn.json`, table and handlers in `apps/server/src/sim/admin.rs`,
advertised to the client for Tab-autocomplete via `ServerMsg::AdminCommands`. `/give` is the one command that writes a
player's stocks, and it hands out ESSENCE rather than blocks on purpose — a block is an item that can be placed, so
gifting one would be a world edit with no journal entry behind it and a slot to reconcile, while essence rides the
standing `InventoryFull` rebase and no screen has to know a gift happened. It parses essence names off `Essence::name`,
the screens' own label, so the command and the UI cannot drift about what a currency is called.
`just play`/`just
play-web` grant `DEV_ADMIN_ID` (the dev sign-in's derived id for subject `admin`, pinned by an e2e
test). Compass in `/tp`: east = sunrise = −x, north = −z.

Dev gotchas: use `127.0.0.1`, never `localhost` (macOS resolves it to `::1`; the server binds IPv4 only — and sign-in
redirects back to whatever origin the page is on, which the master allows by IP literal). Anything that can't open a
browser signs in through `POST /api/dev/signin` — one account per subject string, honoured only under
`ASPNETCORE_ENVIRONMENT=Development`, and a master with it set anywhere else **refuses to boot**. The dev master also
needs `Master__Probe__AllowPrivateTargets` (the SSRF filter refuses non-public addresses) and
`Master__Dev__ProbeHostOverride=host.docker.internal` (the master redirects its OWN probes; this replaced the
registrant-supplied `probeHost`, which let a caller name the only address anyone verified). The justfile's
`DEV_MASTER_ENV` is the one copy of all three. If a client times out against the docker stack, read the server
container's logs before blaming the network: a server dead at boot looks identical. Extra windows:
`just
run -- --master-url http://127.0.0.1:5200`, add `--join --dev-signin <subject>` for scripted flows.

### Capture, bench & metrics flags

For `just run`:

- **Scene/pose**: `--seed <f64>`, `--pos x y z` (the scenes this engine is slow in are NATURAL water, terraced and
  cascading — no synthetic plan builds those), `--tod <deg>` freezes the sun (dev builds, or release **with `--exit`** —
  a run that ends itself is a capture, not a session), `--draw-dist N`, `--mobile` (the iOS config), `--workers N`.
- **Determinism**: `--fixed-dt S` steps the world by exactly S per frame. Required for any A/B — everything animated
  rides dt, so without it two builds of different speed reach `--frames N` in different world states and the diff
  measures the speed difference.
- **Output**: `--screenshot out.png`, `--frames N`, `--exit`, `--metrics-csv`, `--overlay` (backquote toggles),
  `--no-vsync`. `--frames` means two different things depending on what else is on the line: WHEN to take the
  screenshot, and — under `--bench`, or under `--exit` with no screenshot — when the run stops. `--exit` is now fatal
  when nothing can end the run, because the combination that reads most like a capture
  (`--metrics-csv out.csv --frames
  N --exit`) used to arm neither exit path and ran until it was killed, writing CSV
  rows the whole time.
- **Motion**: `--fly N` vs `--warp N` — fly the bench path N frames then freeze, vs land on that same pose with no
  motion behind it. That pair is the only way to photograph a TEMPORAL artifact; every other capture is a still, which
  is exactly when a temporal artifact has already converged away. `--walk` swaps the flyover for an eye-height
  oscillation one block forward and back: a screen-space artifact scales with SCREEN motion, so flyover and walk are two
  regimes and the walk is the one players report from. `--bench-speed B` sets the flyover's blocks/s — **leave the
  default alone**, every BENCH.md baseline was taken there. That default is a different STREAMING regime, not merely a
  faster one, because the GI upload budget can't keep up and the classify drain drops entries, so a cost that only
  appears there may be budget starvation rather than a leak. Dropping the speed well below it soaks at how the game is
  actually played; `--walk` is not a substitute, since its one-block oscillation never brings new terrain into the
  window. Repeat noise on both paths is far under the smallest difference worth acting on, so consult GI-DESIGN.md
  "Measuring motion" for the current floor rather than guessing. For FLICKER specifically, diff a frame against the one
  exactly one oscillation period later on a `--walk` run at fixed dt: the two then sit at the identical pose after
  continuous motion and the diff holds no parallax — adjacent frames are mostly parallax and cannot separate the two
  (R27). Always pair `--fly` with `--bench`: `--fly` alone silently takes the interactive path — menu up, the user's
  SAVED settings loaded, audio on, master dialed — and is not hermetic.
- **Attribution**: `DIG_STUB=<terms>` (`renderer::stub`) removes one shading term per run so ONE build attributes a
  frame — water `wvert wsun wssr wripple wrefract wflat spray nowater`, GI `gisun gigather ginee giflat giatrous`,
  raster `noscene noraster`. Uniform-branch gated, so an un-stubbed run pays nothing, and a typo is fatal rather than
  silently measuring the un-stubbed build. `noscene` is the CLEAN raster stub (the prepass still runs, so depth, face
  codes and GI are untouched); `noraster` drops the prepass too and is an UPPER BOUND only, because an empty depth
  buffer sends every resolve pixel down the sky early-out. Never subtract the two to "get" the prepass.
- **Scenes for capture**: `DIG_VAULT` (sealed stone vault + torch — NEE, and any look call about a dim room;
  `DIG_VAULT_DARK=1` drops that floor torch, which is what a CARRIED light needs — a room whose only source is the one
  in a hand, and it implies `DIG_VAULT` so the bare form cannot silently photograph open terrain. Keeping the torch is
  the other half of the pair: the same lamp placed and carried in one frame, so a calibration drift between them is
  visible. Neither can see the reservoir's carried-light skew, though — one emitter is not enough; that capture is
  `DIG_TORCHES=8 DIG_TORCHES_WALL=1 DIG_SCREEN=torch`, GI-DESIGN.md R28), `DIG_ROOF` (wall-less plate overhead; pins
  that the beam knows the sun's angle rather than a sky column), `DIG_TUNNEL` (a sealed ~52-block stone bore ahead of
  spawn — the unproven-daylight capture, GI-DESIGN.md R32; its far end sits past the cascades' coverage and the gather
  ray's reach, which no room-sized scene touches. Pair with `--pos` at a low y, since the fade it photographs is
  depth-gated; the same run above `SEA_LEVEL` is the geometry-identical control), `DIG_DIVE`, `DIG_SHAPES`, `DIG_RIVER`
  (flooded basin ahead of spawn + `--pitch -35`, the only pose with a SHADED water surface on screen),
  `DIG_PLAYERBOX=<item>` (a fake remote player facing you, holding that item — `dirt` (the default, and what a bare `1`
  still means) `torch` `stairs` `slab` `door` `bucket` `bucket-full` `siphon` `pick` `sigil` `empty`; the value is a
  HELD-ITEM WIRE id, so the scene walks the real `held_icon` decode rather than being a second code path, and an unknown
  name is fatal — the two glowing tools matter here because a remote's copy is a SECOND ink decode, in players.wgsl),
  `DIG_MINING=<level>` (start there; a level is not a pose, and everything one gates is otherwise unphotographable — the
  strength wall's refusals, dig speed, and the Resonant Pick's two ranks, the second of which brightens the tool's own
  glow. Applied after `DIG_ORE`'s own grant, so an explicit level wins), `DIG_TORCHES=<n>` (an open courtyard lit by n
  torches, cluster set well out and floor running past `LIGHT_RANGE` — the local-light capture DIG_VAULT cannot be,
  since the vault's room is too small to have a far band and one torch is the single count the NEE covers completely;
  `DIG_TORCHES_FAR=<n>` parks emitters where they change only the light list's LENGTH, `DIG_TORCHES_WALL=1` screens the
  cluster so emitters occlude each other), `DIG_CONFETTI=1` (a level-up burst every couple of seconds ahead of the
  camera — the event is rare and transient, so it is otherwise invisible to capture), `DIG_CRACK=<percent>` (pins the
  dig-crack decal at that progress and stands a wall just ahead to hold it — the crack is input-driven and a hold is
  over in a second, so a SWEEP of these runs is the only way to see whether it spreads or just fades in; `--pitch` picks
  the row, and a run at 0 is the geometry-identical control since the shader's crack branch is gated on progress),
  `DIG_SIPHON=<percent>` (pins the essence draw at that progress on the vein under the crosshair — the same sweep
  argument, since one still cannot tell a stream that thickens from one that merely brightens. Unlike the crack it
  cannot stand its own scene, because a vein is derived worldgen and there is no placing one: it RIDES `DIG_ORE`, is
  fatal without it, and defaults `DIG_SCREEN=siphon` for the tool and `--pitch` level so the ray goes through the
  deposit's middle row rather than the floor plate in front of it), `DIG_DOORS=1` (a rank of door pairs just ahead —
  closed, open and edge-on — with plain Lumber columns beside them as the tint reference, and Alchemy raised out of
  `unlock::pickup` so the craft-only leaf is `diggable` and therefore outlines at all; DIG_SHAPES also stands every door
  variant but stands them further out behind stair rows sharing their x columns, which projects straight over every
  lower half, so the seam a door is judged on — one object, two cells — is the one thing that scene cannot show),
  `DIG_SCREEN=menu|inventory|alchemy|transmute|transmuting|skills|hand|bucket|bucket-full|torch|siphon|pick|sigil`
  (opens that page at startup, stocking the inventory AND the crafted tools — the pages are otherwise input-only and
  invisible to headless capture, which is how two look-and-feel regressions shipped; `hand` just stocks the bar so the
  viewmodel is visible, `bucket`/`bucket-full` select the water tool empty or charged, `torch` stocks AND SELECTS a
  torch — `hand` cannot serve, because `reconcile_layout` files by block id and lands Dirt in the first slot — the three
  tool names cycle the roster to that tool so its authored geometry photographs in the hand, the slot and the rail at
  once, and `transmuting` puts a BATCH on the bench mid-cycle, since a paced craft needs a press and is over in
  seconds).
- **Capture gotchas**: `--size` sets logical size but the physical framebuffer follows the display's backing scale —
  verify capture pixel dimensions before comparing frame-time runs. On a multi-monitor desk the window opens wherever
  the desktop last had focus, so **`--display N` (R27) is mandatory for any frame-time bracket**: refresh rate rides the
  screen, and a display-paced frame lets the GPU downclock, which inflates every timestamp in the CSV — an unpinned
  before/after bracket measures the screens (two different `refresh_ms` values across the bracket is the tell). The
  chosen monitor's name, size, scale and refresh are logged, and an out-of-range index is fatal. Physics ticks ride the
  wall clock, so cross-build stills need a long enough `--frames` for both cameras to be settled at rest, or the whole
  diff is a sub-block pose shift. **A windowed capture has to stay VISIBLE on macOS** — a covered window is handed no
  surface texture at all (an occluded `nextDrawable` stalls on vsync, so wgpu's Metal backend short-circuits it), and
  the run then idles on `CurrentSurfaceTexture::Occluded` without ever reaching `--frames N`. New in wgpu 30: before it,
  a capture behind another window still rendered. **A SCREENSAVER counts**, which is how this actually bites — start a
  long bench, walk away, come back to a run that stopped where the screen locked. Nothing warns you; the process just
  idles. **`--offscreen` is the way out**: the engine draws into a texture the shell owns instead of the swapchain, so
  there is no drawable, no vsync and no compositor in the picture, and a capture runs the same with the screen locked as
  with it awake. Deliberately not implied by `--screenshot` — it drops the present wait too, so a frame-time run means
  something else off this path, and every BENCH.md baseline was taken through the surface.

Metrics (`digblock-core/src/metrics.rs`) land in the periodic log line, the CSV, and the overlay; `DIG_SPIKELOG=1` logs
per-phase breakdowns of over-budget frames. **F7** (or `--gi-drill-at N` headlessly) clears the GI's derived state and
re-queues every resident chunk. The overlay's `sky`/`emit` fields climb over a session and come down only when an
emitter genuinely leaves (a chunk evicted, or a carried torch put away) — but that ratchet is **not** a leak: the A/B
(BENCH.md 2026-08-12) shows frame time unmoved across a clear, so a session's rising ceiling is real terrain entering
the window. Don't re-litigate it. Note `gpu_gi_denoise_ms` now sums BOTH à-trous iterations; pre-2026-08-12 CSVs report
the stride-2 pass alone and read roughly half. Streaming work is time-budgeted per frame (`STREAM_BUDGET_MS`) and the
wide shadow cascade re-renders sliced across frames, so streaming can't hitch the render loop.

## UI theme

`digblock-render/src/ui/theme.rs` owns every spacing token and color the screens share (`PAD`, `ROW_H`/`ACTION_ROW_H`,
`ROW_GAP`, `SECTION_GAP`, `GAP_ABOVE_BAR`, `MIN_PANEL_W`, plus `PANEL_BG`, `SCREEN_DIM`, `FILL_BG`) — menu, inventory
and alchemy all read from it, and a constant only one screen uses does not belong there. Three load-bearing rules:

1. **Every token is a logical size**, multiplied by `Style::scale` exactly once at the point of use.
2. **A frame has exactly one text size**, `Style::glyph` (= `scale::glyph_px`), with `scale::FIXED_GLYPH_PX` (hotbar
   counts, debug panel) the only sanctioned exception.
3. **A screen is one panel color over one dim**, menu included.

The last two are pinned by `screens::every_screen_draws_text_at_one_size` (swept across window sizes and scales) and
`every_screen_dims_and_panels_the_same_way`, because both have regressed visibly before — the menu and in-game screens
once carried different panel/dim pairs, and the alchemy screen shipped with an enlarged title and a shrink-to-fit that
scaled its glyph by an arbitrary fraction.

A panel too tall for its window shrinks through `theme::fit_scale`, which steps the WHOLE style down in fixed
increments, re-measuring per rung (`glyph_px` rounds) — never a fractional glyph. `ui_scale` quantizes **only** its
derived half (display density × window size, which shimmers unquantized under a window drag); the user's preference
multiplies in exactly, which is what makes a single slider notch mean something. So the effective scale is NOT on that
quantized grid — don't write a test that assumes it is.

## Skills & XP

Two skills (Mining, Alchemy) over a fixed level range, designed in `SKILLS-DESIGN.md` (M1–M7 are built and merged — ore,
the Resonant Pick and §7.2's grid included; M7 is an open-ended catalog stream, so it ships wave by wave rather than
ever being "done"). Everything shared lives in `digblock-core/src/skills.rs`:

- The `Skill` enum is `repr(u8)` and **append-only** — the index is a wire/persistence format.
- The XP curve is a **pinned integer table**, never `powf` — a golden constant both sides must agree on
  (`xp_curve_table_is_pinned`).
- Award tables (`dig_xp`, `alchemy_xp`) and dig speed (SIM v18): `dig_required(block, hardness, level)` keys on the
  block's `DigFamily` (a `blocks!` `dig`-builder field — Fixed/Soft/Wood/Rock), and a family declares only its TWO
  endpoints, the first and last level (Fixed is flat). One shared `DIG_TAPER` table decides how the journey between them
  is spent, so the SHAPE is authored once and no family can drift from the others. That curve is **sampled at a coarse
  level stride** through `dig_speed_step`, a couple of dozen jumps in a life rather than one sub-tick jump per level,
  and the taper's increments **strictly decrease**, so the first speedup is the biggest a player ever feels and every
  later one lands softer (in felt terms too, not just ticks). Both properties are pinned by
  `every_dig_speedup_is_smaller_than_the_one_before`; step size lives in `dig_speed_step` and nowhere else.
  `hardness_ticks` is the BASE the curve scales, not the duration at any reference level — earlier revisions pinned one
  mid-ladder level to reproduce the shipped hardness table exactly, and **v18's retune ended that**: that level is now
  an ordinary sample like any other. Don't reintroduce an assertion that `dig_required` at some level returns the raw
  hardness, and don't hardcode a hold length in a test — derive it from `dig_required`. The harness's `dig_hold`
  stretches holds by the actual family curve — never assume a fixed multiplier.
- **Letting go of a hold decays it, it does not reset it** — `dig_decayed(progress, required, idle)` over
  `DIG_DECAY_TICKS`, draining at a rate that empties a full bar in exactly that window (so the window is the ceiling,
  not a flat wait), and a hold resumed inside it inherits the remainder. Client prediction, the server's `DigState`, the
  completion gate and offline play all call that ONE function; both ends store the banked PEAK plus an idle count and
  never decrement in place, which is what stops two rounding chains drifting apart. The crack reads the same number, so
  it retreats with the decay. Every seam errs in the server's favour (its `DIG_OFF_TARGET_RESET` decay starts
  deliberately late; `verify_digs` runs ahead of the action queue), so an honest completion is never short — the
  rationale and the jitter argument are in SKILLS-DESIGN.md §5.1.
- Storage is XP-only `u64` totals; level is always derived.

**Every level threshold in the game lives in `digblock-core/src/unlock.rs`** — nothing else may hold an unlock number,
and no caller compares levels itself. The tables are the two grid axes (`shape`, `material`) plus `functional`,
`extract` and `mine`, and the questions asked of them (`can_craft`, `can_extract`, `dig_blocked`). `craft` and `pickup`
are DERIVED, never tabled: `craft(b)` = `functional(b)` or `max(shape, material)` (§7.2's grid rule — see "the material
× shape grid" below), and `pickup` reads `craft` of the drop. Two dig gates, one pattern: `mine` is §5.1's strength
wall, keyed on the **`DigFamily`** — the families ordered soil → timber → rock, so every block of a family opens
together (`RejectReason::SkillGated`, protocol v17); `mine_material` is a SECOND gate for the few substances that must
also be earned (the ores at their own reveal, plus the late Metal and Crystal tiers) and `mine` takes the later of the
two, so a material can only push a block later, never earlier and `pickup` (SIM v13) refuses a **craft-only** block —
`crafted` in its `blocks!` row, meaning worldgen places none, pinned by `worldgen_never_places_a_crafted_block` — below
its own `unlock::craft` level, so a pickup can't route around an unlock (`RejectReason::NotUnlocked`, protocol v19).
`pickup` keys on the DROP (pristine Stone hands out Rubble, so quarrying stays a Mining question) and on craft-*only*,
never craftability — `Wood` has a recipe and grows on trees. Client prediction, the server's DigStart backstop, the
bot's filter and the harness all call `dig_blocked`, which returns the first unmet `Requirement` (Mining before Alchemy)
carrying `Kind::message()` — the ONE wording each refusal speaks, which is why the wire sends a reason and never a
sentence. `unlock::opens(skill, level)` derives what a level hands you from those same tables; the level-up fanfare and
the character screen's teasers both key on it (`every_unlock_level_has_prose`).

**XP is server-computed at exactly two chokepoints** — the dig-credit block in `sim/edits.rs` and `process_alchemy` —
and reaches the client as `ServerMsg::XpGain` carrying the new **TOTAL** (idempotent, reorder-safe; the client takes the
max and animates the delta). Join sync always sends `ServerMsg::Skills`, even empty, right after `Layout`. Offline play
awards through the same core functions in the engine's offline arms only (`complete_dig`'s `_ =>`,
`apply_alchemy_action`'s `_ =>`) — never award in the shared prediction path, or multiplayer double-pays.

Persistence: `inventories.bin` carries a skills blob per record (added in v4, and unconditional from v5 on — the version
floor rose with the key change, so nothing older reaches the reader); local saves carry `skills` in
`state.json`/`WorldSave` with serde defaults. UI: the character screen (`ui/screens/character.rs`, C key + pause-menu
row + `DIG_SCREEN=skills`, member of both pinned theme tests), coalescing `+N SKILL` toasts above the hotbar
(`hud::xp_toasts`), and a level-up feed line + sfx via `Engine::apply_xp_total` — both award paths land there so
presentation can't drift.

## Lighting (RTGI)

Lighting is probe-traced GI and it is the **only** render path — no flag. The raster stack it replaced is deleted
(shadow.rs/shadow.wgsl, light.rs and the Stage-B nibble field, baked AO, all fog, LIGHT-DESIGN.md); with AO gone the
mesher's quad winding is fixed rather than AO-keyed, and the vertex words are repacked (w0 carries position, normal and
corner; w1 the texture layer, uv and the emissive bit — field order and widths live with the packing code).

**`GI-DESIGN.md` is the reference** — data model, pass order, every tuned constant, R8–R27 release notes with their
measurements, and a "What is NOT built" section. Read it before changing lighting. Several plausible ideas in there were
built, measured and **declined**, with the numbers recorded so they don't get rebuilt: a coarse super-brick skip mip, a
per-column heightfield sun-march skip (parked at tag `archive/r19-column-skip`), and widening the voxel window to the
streamed radius.

The shape, in brief: a GPU voxel window plus a coarse texture feed two toroidal ambient-cube probe cascades (compute,
swept in a **permuted** round-robin so convergence isn't a visible band, one NEE sample per ray, rays terminating in
last frame's field for multi-bounce). Probes are a **cache**, not the final signal. A half-res resolve fires two
per-pixel rays: a cosine-weighted **final gather** marched through the window and terminated in the probe cache at the
hit (one ray buys the whole multi-bounce series plus contact darkening), and one **deterministic** sun ray along the
exact unquantized caster, its cone offset static and hashed from sub-block WORLD cells, so a point asks the same
question from every camera. `gi_sun_vis` returns the occluder distance and the penumbra is reconstructed spatially from
it. The result accumulates temporally by reprojecting through `prev_view_proj` — exact for static geometry, so no motion
vectors — and is filtered by `gi_atrous.wgsl`, two edge-stopping à-trous iterations stopping hard on face code and view
distance and softly on each signal's own **analytic** variance. Solids bilaterally upsample it against exact face
normals rasterized by the depth prepass into R8Uint; water gathers its indirect **per vertex** (R17 — the prepass runs a
position-only `vs_depth` sharing the color vs's `water_world` decode so the `@invariant`/LessEqual contract holds) and
gates its two ray marches on their own contribution (R21). Reveals prime from the probe cache rather than restarting at
n = 1 (R15), and the per-pixel NEE **resamples every in-range emitter** through a one-pass weighted reservoir (R27; R20
built the ray and picked the argmax) — which is what lights the block a torch stands on. Direct light has exactly ONE
estimator: the gather returns no emitter radiance at all, and `resample_light` lives in gi_common with the light list on
the shared gather group, because players run it too (R33 — they are not in the depth prepass, so the resolve's half-res
direct term can never cover them; without their own call a body beside a torch read black). Unproven daylight fades with
depth (R32): a march that PROVES open sky answers full daylight at any depth, and every answer given on no data — the
cascades' sky fallback, an exhausted march, the medium's in-scatter — multiplies by `medium_sky_fade`, whose top edge
equals `SEA_LEVEL` and saturates there, so the surface world is bit-identical by construction and only underground goes
honestly dark.

Invariants that bite if broken:

- **The accumulator must be rgba32f** (`gi::GI_ACC_FORMAT`). A half-float's mantissa is too short to resolve the
  smallest update a converged history can make, so it rounds to zero and every pixel freezes where it stood;
  `history_format_resolves_the_accumulator` pins N_MAX against the format.
- **The two lighting pairs have FIXED roles, not ping-pong**: `[0] ←resolve→ atrous(1) → [1] → atrous(2) → [0] →scene`.
  That leaves the once-filtered image parked in `[1]` as next frame's history, which is exactly what SVGF feeds back.
- **History length `n` rides the sun target's g channel**, packed above the face code (exact in f32, pinned by
  `packed_metadata_stays_exact`) because WebGPU's per-attachment byte budget is spent. Every reader masks the code back
  out.
- **The history fetch is bilinear, per-tap validated on face code and view distance, and `n` is two-state** — snapped
  onto the lattice (full history) or `N_MOVING`. Travel is not zero at rest: the f32 round trip drifts a fraction of a
  texel with the camera dead still, and resampling that every frame is what streaks resting surfaces.
  `a_moving_pixel_always_gets_the_spatial_filter` pins `N_MOVING < N_CONVERGED`.
- **No moments buffer.** Both estimators have an analytic variance from data already stored (sun visibility is a mean of
  n Bernoulli trials, so its standard error is exactly `sqrt(p(1−p)/n)`), which saves a whole ping-ponged rgba32f and a
  pass of bandwidth — and cannot drift out of step with the accumulator.
- **Sun thresholds are in RADIANCE** (`SUN_MIN_L`, weight × `light_color`), never a bare blend weight. There is no
  tonemap, so a weight that looks negligible against an HDR sky is a large error in the light it admits.
- **Twilight strength (`light_dir.w`) multiplies the LIGHT, never the shadow.**
  `mix(vec3f(1.0), visibility,
  light_dir.w)` is the shadow-map-era form; against a traced visibility it returns 1.0 as
  strength falls, lighting every caster-facing surface at twilight through walls.
- **Water writes OPAQUE** (R25). The fragment samples `scene_tex` and runs the whole transport across `thick`, so `back
  - surf` already IS the backdrop — alpha-blending it over the same backdrop re-adds an unabsorbed copy. Alpha is the
    falling-sheet streak gate alone (it rides the streak signal and nothing else, sitting at exactly 1.0 on every still
    surface).
- **`gi_dda.wgsl` is the one DDA** and is dependency-free so the compute kernels can share it. It owns the two-level
  brick skip, the sky ceiling test (R18 — a non-descending ray above the window's highest occupied cell is a proven miss
  in one compare), `gi_lateral_max`, and `gi_boxes_hit` (R16: shaped cells classify into synthetic PARTIAL classes and
  every march clips the ray's in-cell span against their boxes, so a slab shadows as a slab and an open door lets light
  through). Every partial cell blocks only where its boxes stand, emitters included — R27 retired the
  emitting-whole-cell rule along with the direct-hit term that was its only reason to exist, so a torch no longer casts
  a full-cube shadow from a thin post. Every march steps BEFORE it tests, since a shaped block's surface lies inside its
  own opaque cell.
- **`medium_common.wgsl` is the one participating medium**: `col·exp(−σd) + L_in·(1−exp(−σd))` with L_in = `sky_base`,
  the same function the sky pass draws, so terrain fades into exactly the sky pixel behind it and the streaming rim
  disappears. Air σ is a Koschmieder visibility distance, water σ is measured beam attenuation, and it is the ONLY water
  extinction — `gi_sun_vis` returns a water PATH LENGTH so the sun ray and the view ray cannot disagree. There is no
  fog, no INDIRECT_K, no tonemap.
- **Per-block tint (`block_tint` in chunk.wgsl) multiplies albedo, never light** (R23). The à-trous filter and the
  long-running accumulator only ever see the lighting buffers, so a per-block step in the light is a signal they are
  built to smooth away; on albedo it lands after the bilateral upsample where nothing can filter it. Derived from the
  world position and face code `fs` already carries, plus **one** vertex bit (`shapes::tint_from_below`) shifts the
  hashed cell one down, and it is set on a door's upper half so a two-cell leaf wears ONE tint instead of reading as two
  planks of different timber. That is an anchor shift, not an exemption — the leaf still varies from the block beside
  it. Structurally excluded from emissive ink.

Light stays two independent block properties: `emissive` (the `glows` builder — fullbright ink, a bit of solid word 1,
chunk.wgsl's early return) and `light` (the `emits(level)` builder — the cast level, feeding the GI material LUT and the
light list in `gi.rs`, nothing else). A third property says WHERE FROM rather than how much: `surface_light` (the
`seams(level)` builder, R31) makes a block radiate from its exposed FACES instead of its cell centre — the difference
between a lamp and a seam in a wall. `gi::classify_chunk` declines to place a point source for one, and the MESHER hands
the renderer merged rectangles instead (`mesher::lights`, riding `MeshData::lights` on the rail `impacts` established),
because the mesher is the one pass that has the ore reveal, face exposure and the neighbour shell all in hand at once.
The four ore rows are the only `seams` today; buried rock must light nothing, and a point light one course behind a wall
leaks through it, since `gi_light_margin` skips more than that course. The Torch (`ShapeKind::Torch`, stateless, baked
in shapes.rs) glows AND emits at its authored level; it is hand-priced in `alchemy::functional_ideal` (a Flora-and-Earth
price set by hand, at a level set the same way) because functional items are priced by hand, NEVER by the volume rule —
it has no material and no `ShapeFamily`, which is exactly how a row opts out of the grid. Torch shadows are hard
point-light shadows by design, cast from `gi::light_origin(class)` — the cell centre for anything unshaped, and for a
shaped emitter the centre of its emissive band (a torch radiates from its flame rather than its handle, not from its
handle; R26). `LIGHT_RANGE`/`LIGHT_GAIN`/`LIGHT_NEAR` are pinned across both shaders by
`resolve_and_probe_agree_on_light_calibration`; `LIGHT_NEAR` is where the point model's near field stops being taken
literally (R22), and `emissive_ink`'s gain — quadratic in the ink's OWN luminance — keeps a source out-reading what it
casts, barely, since that ink is already clamped by the scene target and cannot be raised to follow a brighter pool,
which is what caps `LIGHT_GAIN` where it sits. The corollary belongs to the TEXTURE: **an ink texture is authored for
fullbright, not for lit**, because a pale one has no headroom left to take the gain and clips on all three channels
(R29, the torch's post; the ore recipe's darkened stone body knew it first). NEE's stop-short margin is NOT a constant:
`gi_light_margin` (gi_dda.wgsl, shared so both passes agree by construction) is the light's distance to the farthest
corner of its own cell, because a fixed margin only covers that cell while the origin is its centre — and it is asked
through `gi_light_margin_of`, which answers **zero** for a light with no cell of its own (a flag bit of the entry's `w`
word, `GI_LIGHT_CARRIED`; a carried light's origin is on its carrier's capsule axis, so a wall half a block away is
meant to block it and the general formula would swallow the whole wall) and only `GI_LIGHT_SLACK` for a RECTANGLE, whose
sample point already sits on the rock face with air in front of it. A light entry is a PAIR of vec4s since R31, the
second carrying a surface emitter's extents and facing under `GI_LIGHT_QUAD`; `gi_light_sample` (the shared DDA prelude,
so both NEE passes agree by construction) draws the point and returns what it is worth — 1 for a point,
`cos(theta_l) * area` for a rectangle, times `GI_SURFACE_GAIN`, which is the calibration between a lamp's level as an
INTENSITY and a seam's as a RADIANCE PER UNIT AREA. Punctual entries leave the flag clear and their second half zero, so
a torch's word is what it always was. Both gains are stopgaps for the missing tonemap — which cannot simply be appended,
because the offscreen scene colour uses the SURFACE format, so radiance is already clamped at the scene pass.
`flush_lights` re-selects once the camera has moved far enough, when emitters exceed MAX_LIGHTS; spray and viewmodel
read a camera-light buffer. The fullbright INK is one shared function, `emissive_ink` in `ink_common.wgsl`, prepended to
chunk/viewmodel/players so a `glows` block reads the same in the world, in your hand and in somebody else's
(`every_pass_that_draws_ink_shares_one_gain` pins that nobody shadows it with a local const). An ITEM may also be ink in
one place only: `ItemFace::emissive` rides above the texture index in the item vertex word (`viewmodel::INK_BIT`,
decoded identically by viewmodel.wgsl and players.wgsl, and read by the 2D icons as "no directional shading"). That
exists because `glows` is per BLOCK, which is right for a torch — one material — and wrong for a tool, which is a body
worth shading with one lit part set into it (R30).

**A light a player CARRIES is an ordinary entry in that same list** (R28) — there is no second lighting path, and
neither GI shader knows the difference, which is what gives a carried torch bounce, hard shadows, water response and a
lit hand for free. It hangs at `gi::carried_origin(eye)`, a fixed drop below the eye and a function of POSITION and not
facing: the cell is provably non-solid because the carrier's own capsule occupies it, and mouse-look cannot move it, so
a player standing still with a torch is not a moving emitter and their room converges as deeply as one lit from the
floor. Motion is expressed as re-placement — travel `CARRY_INVAL_STEP` (= one C0 spacing) and it fires the same
`gi_invalidate` a block edit fires — and the trigger is a DISTANCE, never a cell crossing, which on a boundary would
fire every frame for a player going nowhere (`TickLerp::render_pos` is written as `prev + (curr - prev)·t` precisely so
a resting camera is bit-identical and the exact-equality early-out in `flush_lights` holds; glam's own `lerp` is not
identity-exact). **Putting a light OUT re-places it too** — otherwise the probes hold its bounce for seconds after the
list has dropped it — and that covers a swap, a disconnect, and a remote leaving the snapshot. Fires are banked by
`Carried::commit` only once the renderer has actually queued them. Carried fires ride `invalidate_moving`, which is
identical to the probes and differs only in that `resolve_inval`'s screen-cap slots evict per-KIND, so a walking
carrier's steady stream cannot push a world edit out inside its lifetime. Because a carried light moves, `flush_lights`
now runs every frame someone walks, so it SELECTS rather than sorts (`select_nth_unstable_by`, and no ordering at all
under the cap); it needs no reservation, since your own light is half a block from the camera and cannot be the one
truncated. Staleness is compared on the REACH-FILTERED set, or a torch-bearer past `lateral_max` but inside the sync
radius defeats the early-out every frame for nothing.

**Not only torches** (R30): the Essence Siphon and the Resonant Pick `emit` too, so holding one lights the tunnel —
green and blue, because an emitter's tint is the mean of its row's top texture and each wears a texgen orb that is ALSO
the emissive face of its geometry. One authored thing decides how the tool looks and what its light looks like. The pick
is the one emitter that answers to its WIELDER: `gi::carried_light(block, mining)` both raises its level and **whitens
its cast tint** at the rank `unlock::pick_cells` widens the swing at, and both ride the light entry itself
(`Emitter::level`/`whitened`, packed above `GI_LIGHT_CARRIED`; a level of 0 defers to the class, so every placed
emitter's word is unchanged) because they are properties of the holder — two players holding the same tool share a LUT
row and must be free to cast differently. A remote's tool casts at its row's level; their skills are not in the
snapshot. Whitening (`gi_light_tint`, in the shared DDA so both NEE passes agree by construction) is what makes that
step legible at all: what a light delivers is its level times its tint's luminance, and a saturated blue carries so
little of it that raising the level alone reads as MORE BLUE rather than as brighter. It touches only the cast — the
tool's own ink stays the authored blue in the hand and in the hotbar. The pin that holds "a tool is not a torch, and the
pick reads as the upgrade" lives in `texture.rs`, the one file that sees both the rows and the pixels.

## Alchemy & the economy

The economy lives in `digblock-core/src/alchemy.rs` — extraction yields (level-scaled, rising to full at the cap), bonus
lines, waste multipliers (falling toward parity as level rises), extraction pace — all integer math, and
`apply_alchemy(inv, op, level)` is level-aware. Six load-bearing rules:

1. **Nothing the world hands you may be transmuted** (SKILLS-DESIGN §7.5). `unlock::craft` and `alchemy::craft_ideal`
   both open on `BlockType::is_bench_only` — DERIVED, never listed: no block worldgen places crumbles or drops into this
   one. So the loose ground, the felled log, foliage, mined rock and the scooped bucket charge have no recipes, and what
   the bench makes is the worked stuff. Its companion is the XP half: `skills::convert_xp` answers zero for anything
   bench-only, because extraction pays for reducing what the WORLD made and nothing for undoing your own work — without
   it craft-then-extract was a standing converter from essence into levels. The bench's PACE deliberately does NOT
   follow (`alchemy::extract_worth`, split out of `convert_xp` for exactly this): a lumber block is as much substance to
   take apart as the log it was planed from.
2. **The Rubble rule** — Stone/Sandstone `crumbles: Some(Rubble)`, and it is what lets rule 1 have an exception without
   carving one: the world is MADE of stone and still never hands you one, so clean rock stays craftable and §7.1's
   quarry-refine-build loop survives whole. `crumbles` and not `drops` because the conditional half is what shuts the
   loop — a stone somebody PLACED hands itself back rather than crumbling, keyed on whether the cell has ever been
   written (`BlockType::dig_credit`, over the journal on the server and its `EditOverlay` mirror on the client, the same
   bit the ore reveal reads). Below the mason's own unlock it crumbles anyway, which is safe precisely because that
   player could not have made one; `no_one_who_can_make_a_block_can_dig_it_into_xp` is the pin. Read the cell's edited
   bit BEFORE the dig's own write on both ends, or every dig finds itself standing in an edited cell. `DepletedStone`
   keeps unconditional `drops` — every cell naming it was written by an old siphon draw.
3. **No matter creation** (`no_level_ever_creates_matter`): craft cost strictly exceeds extraction yield in every
   essence at every level.
4. **Bonus lines only on recipe-less blocks** (`bonus_lines_never_sit_on_craftable_blocks`) — a bonus on a craftable
   block is an essence pump. Structural now: the recipe-less set is exactly the materials whose `craft_units` is `None`.
   A material keeps that split even when no cell of it is craftable, because the split is the PRICE BASIS every shape
   prorates from — Wood still costs what wood costs, though the felled log itself is something you chop.
5. **The essence indices ARE the migration** — `Essence { Flora=0, Earth=1, … }` keeps the retired Material enum's
   numbering so legacy sub-material pairs decode 1:1. Never renumber.
6. **A functional price must be payable at its own level** (`every_functional_price_has_a_source_at_its_own_level`): for
   every essence a hand-priced row names, some block a player at that level can already dig and take apart yields it.
   `functional_ideal` and `unlock::functional` were already pinned as twins so an item can't have a price without a
   level — but nothing asked whether the price was PAYABLE, and the Resonant Pick's Aether line was not (see the ore
   section). A source has to be a block the world HANDS you (rule 3 makes anything off the bench a sink, never a net
   source), past both walls, paying that essence reliably.

The alchemy screen (K) is two tabs. **EXTRACT**: the real inventory grid plus one extraction slot — a *pointer* to a
block id, never a container; drag a stack on and it drips one client-paced `Convert{count:1}` per
`extract_pace_ticks(block, level)` (`AlchScreen::tick`, driven from `Engine::update`) — the pace is the block's
`extract_worth` against a whole block's worth, so the bench spends time in proportion to what comes out, floored so the
cheapest foliage isn't a wire-op stream. The server's bucket therefore banks **ticks** (one per tick, capped at
`bench_burst_ticks(level)`) and an op spends its own pace. **TRANSMUTE**: family tiles whose icons rotate through their
members on the screen's clock (the outgoing member shrinks and fades away and the incoming one grows in —
`family_preview`; never a cross-fade, which double-exposes two 3D icons' internal faces, and never the hard cut it
replaced); selecting a family lays out member tiles (locked ones dim but selectable, affordable ones badged) under a
detail strip and the tab's ONE `TRANSMUTE` button — CLICK 1 · SHIFT 10 · CTRL 100. A §7.2 material × shape pair is one
more member in an existing family, so the catalog grows without the screen growing
(`every_craftable_appears_in_exactly_one_family` pins the mirror; tile rows wrap through `GridSpec` at a fixed column
count). **The button QUEUES that count rather than buying it**: crafting drips one `Craft{count:1}` per
`craft_pace_ticks(item, level)`, the row under the button carries the batch's progress and what is left of it, and the
button itself becomes its CANCEL. Both drips share ONE `progress` and one server bucket because there is one bench — the
tabs are mutually exclusive and leaving one abandons its drip. Crafting still pays the XP it always paid; what changed
is that it takes the time the work takes, which is what stopped the CTRL multiplier being a hundred crafts' XP in a
frame.

Polish invariants: everything sits on ONE left column; panel height is the max of both tabs, bottom-anchored
(`panel_height_is_the_same_on_both_tabs` — a per-tab height jumped the top edge); the active tab's fill fuses into the
content and the strip's rule breaks beneath it; `widgets::slot` carries hover plus a two-weight ring (`SlotRing::Weak` =
selected family context, `Strong` = the acted-on member); locked tiles dim with an unlock badge while unaffordable ones
stay full-color and the detail strip names the shortfall; a rejected drop explains itself on the preview line for a
couple of seconds.

Wire facts: sub-materials ride every wire/save format as flagged ids (the essence index under `alchemy`'s sub-material
flag bit, with a whole block worth a fixed number of units); costs come from `alchemy::craft_ideal` (grid-derived,
hand-priced for functional items) with the level's waste multiplier on top. `ClientMsg::Alchemy` is NOT client-predicted
— the server answers every op with an `InventoryFull` rebase — and rides the edit action queue so craft-then-place in
one tick debits in wire order; offline runs the same `apply_alchemy`. The server enforces the drip via a per-player
bench token bucket in `process_alchemy`, banked in ticks and charged the op's own pace × count — `extract_pace_ticks`
for a Convert, `craft_pace_ticks` for a Craft — so neither a bulk `Convert{count:999}` nor a bulk `Craft{count:100}` nor
a stream of cheap ops can sidestep it; refusals answer with the standing rebase (nothing debited, no XP). ONE bucket for
both halves, so a client can't run the two drips at once for double the throughput. That gate lives in op validation,
NOT `apply_alchemy`, so the offline economy is identical and unpaced. **A craft that would vanish into a type's cap is
refused before any debit** — `Inventory::add` saturates, so the old Water-only guard let a second Resonant Pick take its
full Metal price and hand back nothing, and the tab's bulk multipliers point at tools too. One number,
`Inventory::headroom`, is both the refusal and what `AlchView::affordable` greys the button on, so the offer and the
rule cannot drift; a full cap is not a shortfall, so the detail strip says ALREADY OWNED rather than naming an essence
that cannot help — and it drops the COST line before the reason, since a three-essence price is long enough to elide the
one thing that line is for.

While ANY UI surface is open the engine synthesizes cursor + left-button from the primary touch at the top of
`Engine::input` — one site gives iOS every screen, and no screen knows about touch.

## Settings & local worlds

The in-game Settings screen exposes draw distance (runtime-applied; connected clients send `ClientMsg::SetDrawDist` so
the server's sync radius follows), **Render Scale** and **Lighting Detail**, an interface-scale multiplier
(`--ui-scale`/`?uiscale=`; stepped between bounds owned by `ui::scale` and re-exported from `settings.rs` for the
launchers; every one of the engine's recompute sites routes through `recompute_ui_scale`), Music/SFX volumes, and an SSR
toggle.

**The two resolution knobs** (`settings.rs`, `renderer::RenderScale`): the world renders into `scene_view`/`post_view`
at `surface × render_scale` and ONE upscale pass takes it to the surface, after which the overlay draws at native — so
UI, text, name tags and the crosshair are never resampled. (Name tags project `view_proj → NDC × size`; the `size` fed
to the overlay must stay the SURFACE size, which is the one way this silently regresses.) GI runs at
`render_size /
gi_scale_div`, an INTEGER divisor (`--gi-scale`, presented as "Ultra/High/Medium/Low"); the ratio rides
`frame.misc.w`. GI scale is a fraction of the RENDER size, not the surface.

Which knob to spend is **measured, not intuited** (BENCH.md 2026-08-12): a notch of render scale and a notch of lighting
detail buy comparable frame time, but the GI notch costs far less picture, because the à-trous filter and the upsample
already reconstruct that signal. So the interactive high-DPI default is **native resolution with lighting detail stepped
down** (`density_defaults`), applied at the first `set_scale_factor` and ONLY when no flag was passed and nothing was
saved. `gfx_defaults_auto` is false on every capture path, so bench/screenshot/drill keep the shipped values and no
baseline moves; it deliberately does not mark settings dirty, so the derived default follows the display instead of
freezing.

Two color targets are mandatory: water samples the scene it blends over, so its destination cannot be its source. And
`blit_pipeline` carries a depth-stencil state (it was only ever used inside the water pass) — the standalone upscale
needs the depthless `upscale_pipeline`, or the pass invalidates the encoder and EVERY pass in the frame is dropped,
which looks like a black window with no HUD rather than a bad blit.

Persistence is a launcher-injected `SettingsStore` (desktop: `data_dir/digblock/settings.json`; web: `localStorage`);
precedence is CLI/query > saved > platform default, and non-interactive runs (bench/screenshot/drill) skip load AND save
so captures stay hermetic. The last-joined multiplayer name persists here too. `Settings`
(digblock-render/src/settings.rs) is serde-free; each launcher owns a serde mirror.

Named local worlds live in `crates/digblock-persist`: the journal (shared `VERSION` — a worldgen retune stales server
volumes and client saves together) plus the `WorldStore` trait. Desktop `FsWorldStore` writes the exact server-format
journal + state.json with throttled atomic writes; web `LocalStorageWorldStore` uses localStorage (not IndexedDB)
because `flush` must run inside synchronous `on_suspend`. Version policy: client saves are **refused, never archived**
on mismatch (delete-only in the UI); the `.vN.bak` archive path is server-only.

World UI is in `menu.rs`. In game the root menu reads the same either side — Back to Game, Return to Spawn, Inventory,
Alchemy, one exit row, Settings — with Leave World standing where Leave Server does; Singleplayer and Multiplayer belong
to the locked title menu, so switching worlds or joining a server goes back through the exit. Both exits share
`Engine::return_to_title` (fresh backdrop seed, relocked startup menu); Leave World calls `close_local_save` first and,
unlike the disconnect path, runs unconditionally, since even a store-less ephemeral world has a title to return to.
`Pipeline`'s optional `EditOverlay` records every `set_block` and re-applies cells before air classification, so local
edits survive hard eviction even in unsaved worlds (restored water re-schedules fluid).

## Text

Two embedded TTFs rasterized at runtime with `fontdue` (`digblock-render/src/font/`): a proportional UI face and a
monospace one used ONLY by the debug overlay and spike log, which align columns with printf padding. Glyphs are
rasterized at the exact pixel size they're drawn at, shelf-packed into one lazily-filled 2048² R8 page, and blitted 1:1
on whole-pixel origins — that 1:1-ness is why the Linear sampler costs nothing and why `glyph_px` no longer snaps to a
coarse pixel grid.

Load-bearing rules:

- Sizes are a **layout box height**, never an em, and a text `y` is the box's TOP (`Face::baseline` derives the
  baseline) — which is what keeps every `(rect.h - glyph) / 2.0` centering site working.
- `Face::run` is the single advance walk behind `measure`, `wrap` and the overlay's `push_text`, so a panel's width and
  the pen the renderer walks cannot drift (`measure_matches_the_pen_walk`). Never compute a width from a character
  count.
- The atlas reserves a small solid-white block whose center is the UV of *every* untextured overlay quad, and only ever
  repacks in `begin_frame` — a mid-frame reset would invalidate UVs already in the vertex buffer.
- Ink legitimately overflows the nominal box (accents above, descenders below), so anything drawing a backdrop behind
  text sizes it from `line_h`, not the box.
- The debug panel's width comes from a fixed `DEBUG_COLS` budget with every field printf-padded, NOT from the current
  frame's longest line — it is right-anchored, so content-derived sizing made it resize and slide on every frame that
  changed a digit.

`em_ratio` in `font/mod.rs` is the one knob for a font swap; `assets/fonts/SOURCES.md` has the subsetting recipe and the
tests that fail if a new face is unsuitable.

## Audio

The engine emits `AudioCmd`s into an injected `Box<dyn AudioSink>` (a `None` sink covers bench/screenshot/drill/iOS).
Pure helpers: `digblock-render/src/audio.rs` (materials, footsteps, swim strokes, ambient beds) and `water_audio.rs`
(waterfall voice reconciliation). Cadence emitters pace by DISTANCE, not time, so a slow walk or a drifting swim still
sounds right; strokes fire only when in water and off the bottom, which is exactly where footsteps stop. Fillers:
`apps/desktop/src/audio.rs` (rodio; positional coordinates scaled by `ATT` to shape rodio's fixed 1/d rolloff) and
`apps/web/src/audio.rs` (WebAudio; one-time pointerdown/keydown `resume()` for Chrome's autoplay rule). SFX are embedded
OGGs (`digblock-assets::sfx`, provenance in `assets/sfx/SOURCES.md`). Music is never embedded: the master serves CC0
tracks (`/api/music`) and both fillers fetch at runtime. Mute: `--mute` / `?mute`.

## Browser client

Chrome via wasm32-unknown-unknown + WebGPU (`apps/web`). Query params mirror desktop flags:
`?seed= ?tod= ?draw= ?ssr=0
?overlay ?name= ?mute ?master= ?join&server= ?workers= ?frames=`; defaults are full desktop
quality; `?master=` defaults to the production master, explicitly empty disables multiplayer + music.

**Chunk jobs run in Web Workers** (`apps/web-worker` + `apps/web/src/jobs.rs`), a small default that drops to none on a
low-core device; `?workers=N` overrides. Workers can't share linear memory, so jobs cross as bytes —
`digblock_core::job_wire`, whose **air-chunk flag is load-bearing**: sky chunks share one `AIR_CHUNK` `Arc` recognized
by POINTER, so encoding them as zeroes would decode into look-alikes and the mesher would start meshing the sky. The
pool is injected through `pipeline::JobDispatch` (the `NetTransport` pattern, so core keeps no platform deps and an
in-process dispatcher tests the path headlessly), and it is best-effort: until a worker reports ready — or forever, if
the browser refuses to spawn any — jobs run inline exactly as they did before the pool existed. `JobDispatch::reset` on
`take_dispatch` is what stops a torn-down world's in-flight column landing as terrain in the next one. Two traps worth
knowing: `adaptive_effective` must NOT be gated on the executor (its budget is one deadline over jobs, drain AND
uploads, and only jobs move to the pool — gating it dropped absorption to the fixed floor the moment workers turned on),
and the worker's bootstrap is generated into a blob URL rather than shipped as a file, so the bundle stays exactly two
stamped pairs and never gains a cacheable `.js` that can drift from the module it imports. `std::time::Instant` is
`web_time::Instant` workspace-wide; GPU init is async (`WaitingForGpu` state); the depth "copy" is a blit into R32Float
(WebGPU forbids depth32float as a copy destination); shadow taps use `textureSampleCompareLevel` (Tint's
derivative-uniformity rule). Multiplayer: `crates/digblock-net-web` implements `NetTransport` over WebTransport with
`serverCertificateHashes` from the master; the server's self-signed cert is short-lived, inside Chrome's cap on pinned
certs, and rotates in-process well before it expires, re-registering so the master hands out the fresh pin.
`apps/web/src/services.rs` is the fetch()-based `MenuServices` filler. Scripted joins go through
`EngineConfig::auto_join`, which walks the real menu path. Screenshots, drill, and metrics CSV are native-only; `?bench`
works here now — `?frames=N` ends the run and the summary is logged from `Game::on_exit`, because winit's exit on wasm
unwinds through JS and `Drop` may never run, which is why `?bench` printed nothing here for so long.

Deploying: `just build-web` is the bundle, and `deploy-web.yml` runs that recipe rather than mirroring it, because the
recipe owns a correctness invariant a copy would drift from. The glue JS and the wasm are one artifact (wasm-bindgen
mangles import names with a per-build hash), so a browser pairing fresh wasm with stale glue dies at load with
`LinkError: ... requires a callable`. **Cache headers cannot fix this** and twice appeared to — the CDN in front of the
custom domain rewrites Cache-Control for `.js` and not for `.wasm`, reintroducing the same fatal pairing one layer out.
The fix is naming: both files carry the same content stamp, only the always-revalidated `index.html` knows it, and
`_headers` just declares the pair immutable. A stale index can therefore only 404 on a pair that no longer exists —
self-healing on reload — never link half of a live one.

## Architecture

Workspace with eight crates, two thin app launchers, and a dedicated server. The crate boundaries are load-bearing:

- **`crates/digblock-core`** — pure game logic (worldgen, chunks, meshing, fluid, player physics, input driving, sun,
  metrics). No GPU or windowing deps; everything testable headless.
- **`crates/digblock-platform`** — the winit shell around a headless-tested lifecycle state machine (`lifecycle.rs`),
  input translation, screenshot capture. The **only** crate allowed to touch `winit` or `wgpu::Surface`. Defines the
  `Game` trait and `Gpu` (process-lifetime device/queue).
- **`crates/digblock-render`** — the engine: world state + wgpu renderer, implements `Game`. Renders into
  `&TextureView`s borrowed per `render()` call and never sees the surface, so it can't hold a stale swapchain texture
  across a suspend. `engine.rs` (state/streaming/orchestration) + `renderer.rs` (device-lifetime state, passes) +
  per-system modules (`sky.rs`, `water.rs`, `spray.rs`, `players.rs`, `overlay.rs`, `menu.rs`/`chat.rs`,
  `audio.rs`/`water_audio.rs`, `smoothing.rs`, `item_geom.rs`/`viewmodel.rs`, `ui/`).
- **`crates/digblock-assets`** — compile-time embedded textures, WGSL, and the two UI typefaces (byte-identical on every
  platform). Shaders are validated **twice, by two different front ends, because one was never enough**. `cargo test`
  runs naga, which needs no GPU and is the PERMISSIVE end; `just check-shaders` (inside `just check-web`, the
  `tools/shadercheck` crate) hands every entry of `shader::ALL` to a headless Chrome and reports what Tint says. Chrome
  rejects things naga accepts and the failure is a whole invalidated pipeline in the browser only, which shipped twice —
  `pass` as a reserved word, and `highlight.wgsl` mixing `*` and `^` unparenthesized, costing the outline and the dig
  crack on web while every native gate stayed green. Both gates read the same `ALL`, so a shader cannot be in one and
  not the other. A real browser rather than a pinned `tint` binary on purpose: Dawn publishes no standalone tint, and a
  pinned one drifts from the Chrome players actually run. Two things it still does NOT cover — it compiles MODULES, so
  pipeline-level binding mismatches belong to `every_pipeline_builds_without_a_gpu` (digblock-render, wgpu's noop
  backend, also naga); and neither gate runs the page, so anything beyond shader compilation still wants one live load.
- **`crates/digblock-net`** — protocol + client netcode, **sans-IO** (wasm-clean). Postcard wire format, append-only
  evolution, `PROTOCOL_VERSION` bumped on breaks. Holds the movement predictor, pending-edit tracking, remote-player
  interpolation, and a loopback transport for plain `cargo test`.
- **`crates/digblock-net-native`** — wtransport (WebTransport/QUIC) on a background thread, bridged over non-blocking
  channels. Only desktop and the bot link it.
- **`crates/digblock-persist`** — persistence shared by server and client saves: journal format (index/codec/snapshot
  wasm-clean; file writer + compaction cfg'd native), `WorldStore` trait, `FsWorldStore`. Sits above core AND net. Web
  store impl lives in `apps/web/src/store.rs`.
- **`apps/desktop`**, **`apps/ios`** — thin launchers. Desktop also owns the menu's HTTP services (`services.rs`); the
  engine only sees `MenuServices` channels.
- **`apps/web-worker`** — the browser's chunk-job worker: a cdylib over `digblock-core` ALONE, exporting one function
  (`run_job_bytes`). Separate from `apps/web` on purpose — that module's `#[wasm_bindgen(start)]` reaches for `window()`
  and would panic in a worker, and every worker instantiates its own linear memory, so a worker module an order of
  magnitude smaller than the client is the whole point. Never give it a `start`, and never let a dep in that pulls
  `web-sys`.
- **`apps/server`** — headless dedicated server. Authoritative fixed-tick-rate sim (`sim/`, socket-free, tested by
  injected events), wtransport layer (`net.rs` — verifies join tokens with the master before the sim hears about a
  connection), master HTTP client (`master.rs`), chunk store, append-only edit journal with boot compaction (terrain is
  never sent; join sync = seed + edits; old-version journals archive to `.vN.bak` — this destructive path is
  server-only).
- **`apps/master`** — .NET 10 minimal API, not in the cargo workspace. Four first-party packages and no more: EF Core +
  SQLite (accounts — the master's only durable state), its design-time tools, and the Google/Facebook auth handlers;
  rate limiting needs none, it ships in the shared framework. In-memory registry + one-time token store beside the
  database; verifies reachability with a mandatory QUIC probe pinned to the registered cert hash (no probe-off mode;
  hosts without UDP egress or libmsquic fail at boot). The Rust e2e tests boot its real container. Never put `Urls` in
  appsettings.json — it overrides `ASPNETCORE_URLS` and breaks the container bind. **EF Core turns SQLite's WAL on by
  itself**, and WAL does not work over SMB, which is what the production volume is — `Data/JournalModeInterceptor.cs`
  pins DELETE back unconditionally. Nothing above `Data/` may assume SQLite (no raw SQL, no SQLite-only types), so
  `UseSqlite` is the one line that changes to move to Postgres. The HTTP contract in `Contracts.cs` is mirrored by
  `apps/server/src/master.rs`, `apps/desktop/src/services.rs` and `apps/web/src/services.rs` — change all four together.

### Multiplayer model

Joining is account-authenticated and token-mediated (`AUTH-DESIGN.md`): the client presents its SESSION to the master
(`POST /api/join`, Bearer), gets a one-time token, sends it in `Hello`, and the server consumes it via
`POST
/api/tokens/verify` — which answers with the display name **and the `AccountId`**. Verification fails **closed**;
connected players are unaffected by a master outage.

**The client never asserts who it is.** `Hello` carries the opaque token bytes and nothing else about identity (v23
removed the old `identity` secret); the account arrives from the master and rides `SimEvent::Connected`, so nothing on
the wire can be mistaken for a claim. The one exception is deliberate, bounded and temporary: `ClientMsg::ClaimLegacy`
(v24) offers the key a PRE-ACCOUNT character is filed under, so a player can bring one across per server — see
AUTH-DESIGN.md §6 for the merge rules, the one-time-ness, and the operator exposure it accepts. `AccountId`
(`digblock-net/src/protocol.rs`, hand-parsed UUID, wasm-clean) is the key for `player_store` records, the value
`--admin` compares, and what a ban records. One account holds one live connection — a second is refused, since two would
stomp each other's persisted record, which is why every deliberate exit calls `NetClient::disconnect` (`ClientMsg::Bye`
PLUS a bounded flush: the message alone only reaches a stream buffer, and dropping the transport closes the connection
before it drains). `Game::on_exit` is that hook at the platform edge, kept apart from `on_suspend` because a
backgrounded phone may come back. On web the departure arrives as winit's `exiting` (it splits `pagehide` by
`persisted`: back/forward cache → `Suspended`, an actual close → `LoopExiting`), so wiring a goodbye to suspend alone
would miss the common case of closing a tab. `DEFAULT_MAX_CONNECTIONS_PER_IP`
(`--max-conns-per-ip`/`DIGBLOCK_MAX_CONNS_PER_IP`) caps how much of the connection table one address can spend, which is
what stops the pre-Hello window being an amplifier against the master. It is a flag because every local soak is the
opposite case — `just bot -- --bots N` and `just compose-bots` put every client behind ONE address, and a bot refused at
accept is indistinguishable from a connect timeout.

Server-authoritative everything, but movement uses an **adoption envelope**: clients send per-tick inputs plus claimed
post-tick state; the server runs the same `Player::tick` and adopts claims within `POS_TOLERANCE` that aren't inside
geometry — honest clients never drift or rubber-band. A time-integrated EWMA of claim−sim deltas
(`DRIFT_ALPHA`/`DRIFT_TOLERANCE`) suspends adoption under sustained directional bias, closing the edge-riding exploit.
Rejected claims are corrected by the next snapshot via rewind/replay, surfaced as a briefly decaying camera offset,
never a snap.

The camera is presentation-only in two more ways, both visual-only — gameplay (occupancy via `body_pos()`, the wire,
saves) always reads the authoritative `Player::pos`: it interpolates between the last two tick states (`smoothing.rs`;
without it frame-pacing jitter *is* position jitter, measurably the dominant stutter source — see NET-AUDIT.md), and a
rewind must carry the lerp anchor with it. `NetEvent::Ack` is dispatched ahead of `pump_net`'s edit-budgeted queue: it
has no ordering dependency on edits, and an ack delayed past the predictor's `BUF_CAP` forces every later snapshot down
the replay path. Dig/place is predicted locally and validated server-side by re-raycasting from the claimed origin.
Fluid runs only on the server; results broadcast as ordinary edits. Protocol constants live in
`digblock-net/src/protocol.rs`.

**Items** (protocol v10, extended in v12): the hotbar plus a fixed-size inventory grid (`digblock-core/src/inv_grid.rs`;
the size is a TEST — `the_grid_holds_every_type_a_player_can_own` — because §7.2's materials pushed the roster past the
old grid and a player owning one of everything had types with nowhere to go; `from_wire` pads, so a save written at the
old size decodes into the current one with its arrangement intact) are player-authored *assignments* over the
count-based inventory — a type lives in exactly one slot, kept coherent by `reconcile_layout` (deterministic, run
identically client- and server-side after every count change). The arrangement rides
`ClientMsg::Layout`/`ServerMsg::Layout` on the reliable stream (in-order with edits, via the action queue) and persists
per ACCOUNT in the server's `player_store` (`inventories.bin` v6 — same record body throughout; v5 changed the 16-byte
key's MEANING from a device-secret hash to an `AccountId`, and v6 added the second section that keeps the old ones
readable, so the archive path is corruption-only again and every version this project wrote still loads) and in local
saves' `state.json`. Enforcement: `EditRequest.slot` declares the hotbar slot used, and Place/Collect must match its
item (`RejectReason::WrongSlot`) — honest clients can't race it because Layout precedes the edit on the same stream.
Snapshots carry each player's held item (`RemoteState.held`; the struct split out of `NetPlayerState` at v14), drawn in
a remote's hand as its REAL geometry — `held_icon` routes the wire id through the same `hud::item_icon` the hotbar uses,
and `players.wgsl`'s second pipeline draws `item_geom` through `viewmodel::build`, so a remote's bucket shows its charge
and their torch is a torch that glows and lights the room. It used to be one small cube textured with whatever
`tex_side` the item happened to have. A slot whose count reaches zero is **released**, not greyed; that is safe only
because the engine reconciles and mirrors the layout BEFORE the frame's clicks send edits.

**Tools** (SKILLS-DESIGN §6): item-only `blocks!` rows (`BlockType::Bucket`; `is_tool`, `always_owned`) — never
rendered, placed, or slotted. The tool slot is PINNED to the bar's last position (a legacy mid-bar layout migrates on
`Hotbar::from_wire`); WHICH tool it holds is `Hotbar::active_tool`, carried as the Layout wire's trailing entry, so a
cycle-then-scoop validates in wire order and the server's gates check `active_tool == Bucket`. That entry is
CLIENT-DECLARED and `from_wire` believes any tool id, so ownership is re-asserted in `reconcile_layout` — the one place
that sees the bar and the counts — against `Inventory::owned_tools`, the single roster the rail also reads. Without it a
crafted Layout is enough to swing a pick you never made; the bucket was unforgeable only because it is `always_owned`,
which stopped being the whole roster. Tapping 9 with the tool slot already selected cycles the owned roster
(`Engine::owned_tools`, block-id order); `hud::tool_rail` draws the non-active owned tools as faded small slots right of
the bar, only once a second tool exists. Three presentation rules hold the slot together, and each has already been
broken once: it **stands apart** — `hud::bar_slot_rect` adds `TOOL_SPLIT` before the last slot at DRAW time, leaving
`row_grid`'s nine columns (which every panel measures against, and the inventory grid drops into) uniform — the split
folds into the row's **centring**, never into its spacing, so the lattice keeps one cell and one gap throughout and
simply sits half a split further left. Centring it has to know about the split even though spacing must not: `row_grid`
centred the bare lattice for a while and the tool slot, drawn just past its right edge, hung off the window at any size
below a threshold — with the one test that measured the DRAWN last slot rewritten, in the same change, into
`left - left
== 0`; the exchange is a **handover**, not a cut — `hotbar::ToolSwap` shrinks the outgoing tool away and
grows the incoming one in through the shared `ui::handover` curve the transmute tiles use, and everything else about the
swap (the rail's membership) happens at the invisible midpoint; and every path that names a tool draws it through
`hud::tool_icon`, **`item_icon` included is not enough** — `HotbarItem::WaterTool` names the SLOT, so the viewmodel has
to ask which tool is in it or the whole roster is held as a bucket. The snapshot wire is the same trap and needed no new
encoding: `Client::held_wire` sends the active tool's own block id (sentinels reserved for the bucket's two charges,
exactly as `Hotbar::to_wire` does), and `held_icon` admits a tool id EXPLICITLY rather than by relaxing `is_slottable` —
which answers a different question, whether a GRID slot may hold it — then hands it to `item_icon`, whose own tool arm
forwards to `tool_icon`. So a remote swinging a pick draws the same authored geometry its owner's hand does, and no
second texture-borrowing path exists to disagree with it. Tools carry no texture layers, so a row without an `IconKind`
draws as a block of whatever sits at the first layer; `DIG_SCREEN=pick|siphon|sigil` is how each one reaches a capture
at all, since the bucket is the only tool a fresh player owns.

**The water tool is stateful** (protocol v12), and its charge is stored as the inventory's Water count — capped at one
by `inventory::cap`, named by `Inventory::bucket_full`. Storing the charge in the inventory rather than a field of its
own is the load-bearing decision: prediction, rejection rollback (`inv_delta`), `InventoryFull` rebase and both
persistence formats then carry it with no new machinery. Water is no longer an item — `is_slottable` is false and
`reconcile_layout` sweeps it out of bar and grid whatever the count says, which both keeps it off the item screens and
*is* the migration for saves written when it was a stack (a legacy stack decodes to one full bucket, never a windfall).
**Empty**: the fluid-aware ray (`scoop_ready` picks it), left-click holds to scoop, right-click does nothing.
**Charged**: the ordinary ray — so the crosshair highlights normal blocks again — left-click does nothing at all,
right-click pours through the ordinary `apply_place` path where the stock debit is what empties it. The pour only works
because the ray switched: the fluid ray stops INSIDE a source, leaving no face normal to build against. Server-side both
halves are one affordability gate — `RejectReason::BucketFull` for a scoop with no room (refused BEFORE the world write,
so a full bucket can't delete the spring for nothing), `NoStock` for a pour with no charge — and `WaterTool` is the only
slot that may place water. The charge is geometry, not a tint (`item_geom::bucket_faces`), so it reads in the hotbar
icon, the hand, and on remote players; `viewmodel::same_item` deliberately treats the two states as one item so filling
the bucket doesn't dip the hand like a slot change.

**Your own held item is the viewmodel** (`viewmodel.rs`): authored in view space with its own narrow projection, so no
camera basis, no world-FOV coupling, and no near-plane clipping. It draws in a fifth pass between water and overlay that
CLEARS the shared depth attachment (the water pass discards it anyway) — that is what makes the item immune to walking
into a wall while still self-occluding, which a non-convex item like a staircase needs. Geometry comes from
`item_geom.rs`, the one authority shared with the inventory icons; corners follow `mesher::face_corners` because both
paths draw through `mesher::quad_indices`. The bob paces off horizontal distance (like `Footsteps`) and is advanced in
`update`, NOT `step_audio`, which early-returns with no audio sink and would freeze the hand in exactly the runs that
photograph it. `Swap` owns which item is DRAWN — it lags `hotbar.selected` by half a dip so the exchange happens below
the frame.

### Blocks (core/block.rs)

Block behavior is data-driven via the `blocks!` macro — one row produces the `repr(u16)` enum, `COUNT`/`ALL`, and the
`PROPS` table together, so they can't skew. Rows hold `RenderClass` (None/Opaque/Cutout/Translucent/Shaped), `collides`,
per-face texture layers, `MeshBucket`, and the grid's `material`; water is a block family driven by `FluidProps` data.
Adding a block = one row + PNG + `LAYERS` entry.

A row states what a dig hands back in exactly one of two ways, never both (`table_sanity`): `drops(x)` is unconditional,
and `crumbles(x)` applies to the WORLD'S copy alone — see the economy's rule 2 for why that conditional is what keeps
clean rock craftable. `dig_drop()` is the pristine answer and the question every economy rule means ("what can a fresh
world put in a player's hands"); `dig_credit(edited, alchemy)` is what an actual dig pays.

The layer index is RUNTIME ONLY (mesh vertex bits, never the wire or a save), so the list may be reordered — what it may
not do is drift from `digblock_core::block::tex`, which mirrors it position for position and would silently texture
blocks with each other's PNGs; a `const` assert in `digblock-render/src/texture.rs` (the only file that sees both
crates) fails the BUILD on a length skew.

Texture vocabulary: **`LOG`/`LOG_TOP` is the tree** (bark, cut-end rings) and the **`LUMBER_*` set is planed timber** —
the material crafted things are made of (`LUMBER_LIGHT` is the joinery — the bucket, the furniture, and the `Lumber`
block with its stairs, slab and door; `LUMBER_DARK` and `LUMBER_RED` are the second and third timbers, `DarkLumber` and
`RedLumber` each arriving at its own Alchemy level with a full block/stairs/slab/door set — a handful of rows and no new
art, which is what a material costs once the grid derives its levels and prices. `LUMBER_DARK` also stands in for metal
in `item_geom`: the pick's head, the siphon's funnel — and it is the **torch's post**, because that texture is drawn
fullbright and a pale timber has no headroom left to take `emissive_ink`'s gain: it clipped on every channel and the
post out-read the flame standing on it). Calling both "wood" is what made it unclear which one a row was asking for.
`Log.png` is seam-fixed for VERTICAL tiling (trunks stack); `GrassSide`/`DryGrassSide` deliberately do NOT tile
vertically (dirt below, green fringe above), so a blanket tiling test is not possible. Derived textures come from the
checked-in `tools/texgen`, outputs committed — a run rewrites EVERY output, and the noisy recipes share one rng, so
adding or removing a `save` rerolls the noise in everything after it.

Chunk cells are `u16` end-to-end: 10-bit id, spare bits 10–11, STATE nibble in 12–15, travelling wire and journal as-is
(protocol v8, journal v3). Mesher, physics, fluid, and raycast all key off the tables — no per-block special cases
elsewhere.

### Sub-block shapes (core/shapes.rs)

Shaped blocks live on a **uniform sub-grid**, fine enough to divide evenly into halves, thirds and quarters, geometry
STATIC per (block id, state nibble) — no per-instance micro-voxels. `shapes.rs` authors each shape as sub-box lists in a
canonical +x facing and bakes every variant at first use (rotate → rasterize → greedy-merge; pure integer,
deterministic) into render quad templates, boundary coverage masks (`FaceMask`, the culling currency), collision boxes,
and interaction boxes. Per-shape geometry lives in `shapes.rs` comments and pinned tests — don't re-document it here.
`state_valid` is THE wire gate for state bits; `Facing` is axis-named (`Dir::West` is +x — compass names are a trap).

Mesher: shaped cells stamp templates via `emit_shaped`; boundary quads cull against `neighbor_cover`; `MAX_SHAPE_QUADS`
(test-pinned per variant) sizes the renderer's `MAX_QUADS`. Vertices pack positions in twelfths (a fixed bit budget per
axis); shaders decode with an integer whole/remainder split so lattice points stay exact under GPU fast-math —
chunk.wgsl and water.wgsl must always agree.

Physics (SIM v10): stairs collide as a **diagonal ramp** (`RAMP_SNAP_*`; beyond the band or pinched under a low ceiling
they revert to their boxes and block like walls; the run-out lets a flight mount the flush cube it tops out against,
mirrored in `overlaps_solid` so mid-mount claims stay adoptable). A ramp supports only from its low front edge on, and
the downhill glue takes the highest surface underfoot only when it is below the feet — together those make a flight one
continuous climb (`stair_flight_climbs_at_a_constant_rate`). **Auto-step** (`AUTO_STEP_MAX`): a descending walk mounts
any Cube/Boxes top within that reach of the feet when there's capsule headroom; ramps are structurally excluded and
doors stay unwalkable via the headroom probe alone. A step and a stair are deliberately the same motion — the mount
rises at `MOUNT_RATE` (= `WALK_SPEED`, exactly what a 45° ramp gives a walk), so the tick lerp draws it smoothly with no
presentation special case. Never rate-limit a *landing*, which is why `step_contact` takes only descents slower than the
mount climbs. `step_mount` is the one position-only predicate the resolver, `grounded` and `overlaps_solid` all read (it
requires the axis be OFF the box — a mount is an approach from the side). `overlaps_solid`/`placement_blocked` are the
shape-aware occupancy contract shared by client place-rejection, the server's InsidePlayer scan, and the adoption
envelope's probe — standing on a shaped block is NOT "inside solid". Raycasts narrow-phase against collision boxes (miss
→ DDA continues); `raycast_interact` uses interaction boxes so open doors stay clickable.

Right-click is use-beats-place: an interactable under the ray toggles via `EditKind::Interact`, which rides the whole
existing EditRequest validation/rate-limit/queue machinery (the server flips both door halves). Multi-cell shapes
place/dig as a pair written lower-first in one tick, client-predicted under one seq via `PendingEdits::begin_multi`;
per-cell LWW journal replay keeps pairs consistent. **A door is one object visually, in both places that could betray
it**: `shapes::object_bounds` boxes the pair from either half (anchor = the lower cell, and the box is taller than one
cell — the caller must not assume a box fits inside its cell), so the crosshair outlines the whole leaf and the crack
seeds off one cell; and `shapes::tint_from_below` anchors the upper half's per-block tint on the lower cell. Both are
pure functions of one cell value, deriving the partner's variant the way `door_partner` derives its cell. v1 limits
(documented, deliberate): shaped blocks float when support is dug, fluid treats them as solid, water emits no face
against them.

The **planed-timber set** (`Lumber` + `LumberStairs`/`LumberSlab`/`LumberDoor`, all `LUMBER_LIGHT`) is §7.2's grid in
miniature: the whole set unlocks at the MATERIAL's level, not at each shape's own, which is what makes a material read
as a material. The `Lumber` block exists because its slab has to have something to complete into (`slab_base` derives
that from the material, so a new timber's slab can't ship without its cube). The door is a lumber item too — it was
`WoodDoor` in dark timber at its own Alchemy level, and joined the set when the material arrived (the id is unchanged;
the rename is source-only).

### The material × shape grid

`digblock-core/src/material.rs` is §7.2's substance axis and `block::ShapeFamily` its form axis; a row declares its
substance with the `mat(...)` builder beside `shaped(...)`, and **five things then derive from the pair** rather than
being tabled four times over: the unlock level (`max(shape, material)`), the craft cost (the material's whole-block
essence split prorated UP to the shape's `units()`), the extraction yield (the same split prorated DOWN), the earned
half of the strength wall (`mine_material`, layered over the dig family's), and the transmute tab's families. Rounding
the two sides in opposite directions is what makes "no matter creation" hold for every shape of every material without a
proof per pair, and `craft_units: None` — mined rock, gravel, foliage, ore — IS the recipe-less set the bonus lines are
allowed to sit on.

The BLOCKS shape sits at the very first level on purpose: the progression belongs to the materials, so a substance
arrives in every form you have already learned (Lumber's pieces all together, then every clean Stone form together), and
reaching a shape later hands it to you in every material you already own. `grid_gates_compose` enforces both halves.
Functional items — tools, torches, lamps, the bucket charge — have NO material: §7.3 prices function by hand, and
`unlock::functional`/`alchemy::functional_ideal` are pinned twins so an item can't have a level without a price.
`material_roster_is_pinned` spells out both sets, because a row picking up a material by accident moves four numbers
silently.

**Stackable slabs** (Wood/Stone/LumberSlab): thirds anchored to one end of their cell — one state bit selects the
two-thirds height, another anchors to the ceiling instead of the floor (`slab_state`/`slab_is_top`). `slab_place_state`
keys on the hit normal alone, never a cursor position, which would have to survive client and server rays agreeing to
the twelfth. Placing onto a same-material slab's OPEN face (`slab_grow_normal`) grows the hit cell in place
(`slab_grown`: one third → two thirds → the plain base block, anchor carried along); cross-material hits and hits on the
flush face place a fresh one-third slab in the neighbour cell. The wire always carries the canonical base-state cell and
the server re-derives BOTH the grow and the anchor from its own raycast (a client-sent non-canonical state is BadBlock;
slot/stock gates key on the requested slab even when the written cell is Wood/Stone). The two anchors are exact mirrors,
so one item, one dig time and one alchemy cost serve both (`a_ceiling_slab_is_a_floor_slab_mirrored`). Digging shrinks
one third per completed hold (`slab_shrunk` keeps the anchor, so a hanging slab never drops mid-removal), one slab
credit each; `DigState`/`DigProgress` track the FULL cell, so a grow landing mid-hold resets the timer. The economy
closes exactly: three slabs are worth precisely one whole block. You can't grow the cell you stand in (InsidePlayer —
step off first).

### Ore (core/worldgen/ore.rs)

**Derived, never stored.** A deposit is a salted 3D position hash over a coarse grid (`structures_near`'s apron pattern
in three dimensions), so `build_column` returns the bytes it always returned, the journal carries Stone,
`worldgen_golden_hash_pins_sim_version` does not move, and veins appear retroactively in worlds that already exist.
Depth bands OVERLAP — every tier stays available all the way down and the deep ones are rarer — because exclusive bands
would leave a player at the earliest mining tier finding nothing at all near the surface.

Three rules bite if broken:

- **The unedited rule is the anti-dupe.** Ore manifests only in pristine Stone; without it, placing crafted Stone back
  into a spent vein turns a modest Earth cost into far more Metal than it should, and Metal has no other source.
  `JournalIndex::record` only ever inserts, so "touched" is monotone. The client needs the same answer, which is why
  `Pipeline` keeps its `EditOverlay` in MULTIPLAYER too — but there it MIRRORS the journal rather than the client's
  guesses: `apply_authoritative` records (before the applied/deferred split, so a deferred edit still counts), and
  `set_cell` records only where it is itself the authority (`reapply`, i.e. local worlds). Recording predictions instead
  is unfixable, because a rejection reverts through that same `set_cell` and `record` is insert-only — the cell stays
  edited on the client and pristine on the server, forever. Because a record can change what a chunk DRAWS without
  changing a cell (the reveal keys on "edited"), `apply_authoritative` re-meshes when a first record lands on a value
  that already matched — journal replay re-sending Stone over Stone otherwise leaves a seam glittering in a chunk whose
  dig path calls it spent.
- **The reveal stamps block IDS into `MeshInput`, in the worker.** Not texture layers: the mesher, the texture path, the
  emissive bit, `block_tint` and shaped culling then all see a real ore block and need no change. It runs deposit-major
  in two passes — every deposit CLAIMS its cells in the same grid order `ore_at` walks (skipping unrevealed ones would
  let a later deposit take a cell the dig path says belongs to the first), then claims by an unseen tier revert. Pass
  two touches only the claimed indices, never a sweep, or a placed SPENT rock turns to stone for every visitor below its
  tier (`the_reveal_stamp_leaves_everything_else_alone`, swept across levels — at the cap the second pass reverts
  nothing whatever it does, so a run there cannot tell the two apart).
- **One question answers every dig**: `ore::dig_mode(cell, at, seed, mining, edited, tool)`, asked identically by client
  prediction and the server's own world. Extraction needs no new wire; only the refusal does (`RejectReason::NeedsTool`,
  protocol v21). An extraction is TIMED as the ore (`ore::dig_block`), writes the tier's own spent rock
  (`ore::spent_rock`) rather than air, pays both skills (`skills::ore_field_xp`), and shows no crack.
- **Glowvein carries the first Aether, and the middle tier TRACES it.** Every Aether in the game used to sit past the
  Resonant Pick — the deepest tier revealed far above the pick's own gate, the foliage and loose-rock bonus lines sat
  high on the Alchemy ladder, and Crystal is a craft-only SINK rather than a source — so the pick was unlocked and
  unbuildable for a long stretch. Glowvein's own Aether line closes that without a threshold of its own, because it
  reveals at exactly the level the pick's wider swing opens at. Beside it, `ore::traces_aether` gives a share of
  Duskstone's cells a unit of Aether: a RATE and not a yield line, since the finest an essence split can be cut is a
  whole unit and one per draw is far more than a middle-tier seam is worth. It is derived from the cell and the world
  seed exactly as the deposit is — never rolled — which is what lets `alchemy::field_yield` own it and still be one
  answer for the server, the offline arm and a rejoin years later. The pin is
  `every_functional_price_has_a_source_at_its_own_level` and it counts only what a tier pays RELIABLY: a price payable
  when the hash smiles is not payable.
- **A vein is world geometry and never an item.** The siphon draws the seam out where it stands: it credits ESSENCE
  (`alchemy::field_yield` — the ore's ordinary bench yield, so the Alchemy level scales a field draw exactly as it
  scales the bench, with Mining's capstone multiplying the whole split rather than a line of it), and what a player can
  carry home is the SPENT rock afterwards, one per tier, which drops itself. `is_slottable` is false for all four ores,
  which is both the rule and the migration for saves written while a siphon still handed ore over — `reconcile_layout`
  sweeps a non-slottable type out of bar and grid, exactly as it did for Water. The essence is deliberately NOT
  predicted: it is a split, a `PendingEdit` carries one `inv_delta`, and rather than widen the wire for the one tier
  that pays in two, the extraction takes the path `ClientMsg::Alchemy` already takes and the standing `InventoryFull`
  rebase carries the counts. The CELL is still predicted, so the vein turns spent under the hand. (`DepletedStone`, the
  one grey face all four tiers used to leave, is kept and unreachable — cells written before the per-tier rocks existed
  still name it.)
- **The draw has its own effect, because the crack would lie.** A dig crack is fractures spreading over rock about to
  fall, and a drawn seam leaves the mountain standing — which is why `dig_progress` returns the outline and no crack
  here. `siphon.rs`/`siphon.wgsl` is what it returns it in favour of: motes of the tier's own `ore_flecks` colour lift
  off the struck face and stream into the tool, over a kindle on the face that whitens as the hold builds. Stateless in
  spray/confetti's sense — the CPU owns one instance plus an envelope, and every mote's flight is a pure function of
  that and the shared animation clock — and the ONE effect drawn additively, so it takes the medium's transmittance and
  must never in-scatter sky. Two things hold it together: the motes converge on `gi::carried_origin`, handed over rather
  than recomputed, so the stream ends exactly where the siphon's own glow hangs; and a completed draw flares by starting
  its fade-out above full (`siphon::FLARE`), which is why `SiphonDraw` outlives the hold — the cell is spent rock by
  then, and there is no vein left to ask where the motes were coming from.
- **And its own two sounds, for the same reason.** `Sfx::SiphonDraw` is a resonant loop that replaces the dig loop's
  hammer blows on the SAME hold voice (`LOOP_DIG`), so every stop path — release, cancel, completion, world swap —
  already covers it and the existing pitch ramp reads as the resonance winding up; `Sfx::SiphonTake` replaces the
  `break_sfx` at completion, because nothing shattered. Both were resolving through `material_of` the ore, which is
  rock, so a field extraction used to sound exactly like quarrying one. The take FALLS where both level cues rise —
  shape, not key, is what keeps taking a seam from reading as gaining a level. Recipes in `assets/sfx/SOURCES.md`, and
  those two are the entries whose seeds and Ogg serial are pinned, so a re-run reproduces the committed bytes.

The GI window's class byte is the capacity to watch: `gi::SHAPE_CLASS_BASE` splits the byte between full-cube ids and
shaped covers, a shaped MATERIAL costs a dozen-odd covers, and the pin reports the remaining headroom.
`DIG_ORE=<tier
1..=4>` hollows a chamber around the nearest deposit of a tier and grants its level — the carve must not
touch the vein, or the anti-dupe deletes what is being photographed.

### Tools & the Resonant Pick (core/pick.rs)

Four tools now (`is_tool`, an `inventory::cap` of one, `always_owned` only for the bucket): the bucket, the **Essence
Siphon** (no Metal in its price, ever, or ore and Metal deadlock each other), the **Resonant Pick** (craftable and
usable at the same level in either skill) and the **Master's Sigil** (the last thing either ladder unlocks, priced
entirely in Aether, and it DOES nothing — the point is that it is the catalog's last line). The sigil is `is_tool` for
the mechanics, not for any function: an item-only row you HOLD, capped at one, cycling on tap-9 and drawing in the rail,
kept out of the block slots and the grid by the rule the other three already use. Every path that names a tool goes
through `hud::tool_icon`, because a tool has authored geometry and `IconKind::Cube` would draw it as a cube of its row's
layer — which used to mean WATER (layer 0) and now, for the two that glow, means a cube of bare glow; `SlotView::Tool`
carries WHICH tool is active rather than assuming the bucket.

**The siphon and the pick are light sources** (R30): each `emits` below a torch's level and wears a texgen orb — green
for the siphon, blue for the pick — that is at once the tool row's texture (whence the GI takes the colour of the light
it casts) and the emissive face of its geometry (`glow_faces`, the per-face ink bit). Holding one lights the tunnel, so
mining need not be one-handed. They are deliberately not equals: the siphon is an extraction tool that happens to glow
and throws a pool around the feet, while the pick — the tool you MINE with — brightens AND whitens at its second rank
(`gi::carried_light`, the only place in the game an emitter answers to its holder) into the best light either hand has
short of a torch. The siphon's geometry was rebuilt in the same pass: a narrow nozzle, a tall glowing barrel, a side
spout and a dark grip — its old wide base plate and stub of a stem read as a rubber stamp, and a downward funnel cannot
help, since both views of an item look DOWN at it.

The pick breaks the PLANE of the face it hits — widening once at each of two Mining milestones — derived on both ends
from the ray (`pick::cells` over each side's own world), so the client names one cell and the server decides what the
swing touched. Two rules: every extra cell must yield the **same credit** (`begin_multi` carries ONE `inv_delta`, so
homogeneity is what makes `(block, +N)` correct — a matching block id stopped being enough once provenance decided a
drop), and the hold multiplier is **level-only** (`skills::dig_required_with`) — a world-derived one would let the two
ends disagree by the whole multiplier over a single differing cell, far outside `dig_grace`. A lone block therefore
costs the multiplier too, which is why the spread penalty is set against an offsetting tool bonus. **The reach is a
bonus in material, not in XP**: the aimed cell pays its whole Mining award and every cell behind it pays
`PICK_EXTRA_PERMILLE` of one, floored at a point and capped at the aimed cell's own award (`skills::dig_award`,
SKILLS-DESIGN §5.2).

### Spawn & protected cells (core/spawn.rs + core/protect.rs)

Both modules carry thorough `//!` docs: the two-layer design (seed-only `anchor`, edit-aware `resolve`), why the search
anchors at the pristine spawn and never descends, that nothing is persisted, and protect's one-predicate rule. What
those don't say, and what bites:

- **The `Edits` view is edits only, never a chunk store.** A store answers `None` for non-resident cells and the support
  test reads `None` as "trust worldgen", so wiring one in makes unloaded columns read as void. The server's journal is
  the correct and complete source, fluid included.
- **The anchor's acceptance test is a level disc** — every column within `CLEAR` at exactly `h`, dry, not a cave mouth,
  nothing in the `CAPSULE` cylinder above — and level in *both* directions, since the original rule ignored neighbours
  below `h` and spawns could perch on the lip of a drop. Level is also what makes `edits_clear`'s rules absolute and
  keeps the overlay-empty search bit-identical to the pristine walk. The pins assert against worldgen directly
  (`every_anchor_stands_on_level_ground_under_open_sky`, `resolve_with_no_edits_is_the_anchor`, golden
  `spawn_positions_are_pinned`), because a test that only asks whether spawn *moved* cannot catch a bad destination.
- **`spawn::lock_region` is exactly the cell set the search inspects**, and `stands_clear` reads exactly those cells
  (`lock_region_covers_what_the_test_reads`, `stands_clear_reads_exactly_the_locked_region`). That equivalence is why a
  locked spawn is permanently valid and why `Sim::spawn_point` skips the search when locked; `/lock-spawn on` resolves
  first and locks around the **result**, so it can't freeze an already-broken spawn. `/set-spawn` validates with the
  same predicate, which reads the world as it *is* rather than pristine worldgen, and deliberately skips `SPAWN_FLOOR` —
  an operator aiming at a block has made a choice, and the water rule is what keeps a hand-picked spawn out of the sea.
- **Protection refuses the fluid *write*, not the schedule** — that is what stops water sourced outside a locked region
  flowing in. Rejections are `RejectReason::Protected` (protocol v11); the client picks dig-vs-place wording from
  `PendingEdit.inv_delta`'s sign, NOT `placed == Air`, which a slab dig breaks. A protected hold is refused at
  `DigStart` (`ServerMsg::DigRefused`) rather than at the completing edit, so the player isn't left holding the button
  through a full dig timer, and `refused_cell` stops `step_dig` reopening it every frame.
- **Rejoin resumes in place**, through the same `clear_landing` rule as a spawn: `PlayerRecord` carries the last eye
  position *and* the return-to-spawn tick, since position alone let a rejoin clear the cooldown. `/set-spawn-dir` rides
  `Welcome.spawn_look` and `Teleported.look` (protocol v13), but a resume always sends zero, and the client's feed line
  keys on `spawn_return_pending` rather than on whether a facing came back.

### Worldgen (core/worldgen/ + decoration.rs)

Everything is a pure function of `(x, z, seed)` — column content never depends on generation order. Stages: `terrain.rs`
(continentalness/roughness/ridge/detail control fields, tuning in `TerrainParams`, terraced erosion), `rivers.rs`
(channels from the zero band of a warped field, meeting the sea at grade), `lakes.rs` (sparse bowls in flat inland
country; lake beats river per column), `caves.rs` (intersection of two 3D-noise bands, land only), `classify.rs`
(per-column `SurfaceKind` + climate biomes, block rules/tree density/species as `SurfaceProps` table data; climate
shifts the lines and picks the biome, never the terrain). `SEA_LEVEL` sits midway up a world of fixed height.

Structures are a decoration pass using pure apron evaluation: per-family salted position hashes (`structures_near`),
every chunk within `STRUCTURE_APRON` computes identical structures independently, so they span borders with no pipeline
state; trunk roots are the single exception to decoration's write-into-Air-only rule. Sky chunks resolve to the shared
`AIR_CHUNK` sentinel Arc (never meshed, recognized by pointer).

### Chunk pipeline (core/pipeline.rs + mesher.rs)

The main thread owns the chunk store and scheduling; workers run pure functions (terrain gen, meshing) over immutable
snapshots — a mesh job gets a padded copy of its chunk (a one-block shell from every surrounding neighbour, feeding
water corner heights). Staleness is handled by generation counters: results whose `generation` doesn't match are dropped
(the field was `gen` until edition 2024 reserved that word; the overlay and CSV labels still say `gen`); a new chunk
bumps its own and affected Ready neighbors' counters. Edits go through `Pipeline::set_block`/`set_cell` (copy-on-write
via `Arc::make_mut`, bumping only neighbors whose shell contains the edit). Vertices are two u32 words: word 0 geometry
(positions in twelfths), word 1 material (a texture layer index bounded by `MAX_TEX_LAYERS`; water carries its
surface/flow payload in the upper bits).

**Water surface knitting** is the subtle part; the rules are commented at the code. `mesher/water.rs::corner_drop` is a
pure function of the horizontal lattice corner — never of the cell emitting the face — which is what lets two cells
sharing a corner agree and knit without a crack, including across chunk borders. Its pins, in order: water above or a
covered cell → full; an air cell with water below → plunge the whole block, UNLESS a resting source touches the corner;
else the complement of the **strongest** touching amount. Strongest, not the mean: the sim halves the amount on each
spread, so averaging drags every corner toward the thinner water beyond it, while the max makes each cell's top face
span exactly from its uphill neighbour's level to its own. `water.wgsl` then maps that code to a height **linearly** —
the two halves are one invariant, and a hand-tuned curve simply falsifies the codes the sim computed, so any change to
how corners are chosen must be checked against the mapping and not just the codes. The source-beats-plunge pin is
separate and still needed: only water resting on a **colliding** block spreads outward at its own level (sources
included), so a bucket emptied against a wall face is ringed by air over the pool it just poured — the plunge trigger on
all four corners at once, which without the pin loses the block's top face and skips every side as zero-area.

### Lifecycle robustness

Suspend/resume and device-loss are standing regression targets for a future Android port. `Renderer::new` is a pure
constructor so device-loss recovery is "build a new one and re-upload meshes" (`Game::rebuild_gpu_resources`).
`Game::on_suspend` must be cheap and synchronous — the process may be killed with no further callbacks. Test via
`just
run --drill` or F9/F10 at runtime; F8 screenshots.

## iOS notes

`apps/ios/xcode/DigBlock.xcodeproj` is generated from `project.yml` — edit the yml and rerun `xcodegen`, never the
project file. Simulator/device builds need full Xcode; `just check-ios` is the fast gate that doesn't.
