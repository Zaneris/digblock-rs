# Bench baseline & hotspot analysis

## 2026-08-14 — web chunk workers: the first scripted web bench

The first `?bench` numbers ever taken in a browser. Chrome/WebGPU, M2 Max,
120 Hz, `?bench&fly=1&frames=1500&seed=42&mute`, draw 20, lighting detail /3
(the density default at 2× display scale). `?workers=` picks the arm.

| | `workers=0` | `workers=2` (shipped) | `workers=4` |
|---|---|---|---|
| fill | **n/a — never, in 12.5 s** | **11.91 s** | **4.63 s** |
| avg | 8.33 | 8.33 | 8.34 |
| p95 | 10.60 | **9.30** | 9.40 |
| p99 | 11.10 | **9.70** | 10.20 |
| worst | 11.80 | **11.00** | 15.50 |
| over-8.3 / over-12.5 ms | 461 / 0 | 433 / 0 | 439 / **4** |
| gen jobs | 138 (11/s) avg 2.29 ms | **1245 (100/s)** avg 1.29 ms | 1245 avg 1.17 ms |
| mesh jobs | 5630 (451/s) avg 0.14 ms | **43644 (3497/s)** avg 0.10 ms | 33367 avg 0.08 ms |
| busy (overlay) | 1.02–1.04 ms | 1.24–1.40 ms | 1.07–2.01 ms |
| uploads / 5 s | ~25 MB | ~200 MB | ~350 MB |

**4 workers was measured and declined.** It fills 2.6× faster than 2 and
still beats the inline path on p99 (10.20 vs 11.10), but it is the only arm
that drops a frame at all (4 over 12.5 ms, worst 15.50) — and this change
exists to buy frame time, not to spend it. Shipping 2. Note the shipped cap
is not what protects weak hardware: `default_count`'s `cores - 2` is, so a
4-core machine gets 2 regardless and a 1–2 core machine gets none. Raising
`MAX_WORKERS` only ever affects machines with cores to spare.

Also note 4 workers does FEWER mesh jobs than 2 (33k vs 44k): a faster fill
lands stack siblings together, so fewer cross-border remesh cascades fire —
the same effect the second pass's column-gen rework measured natively.

**Every frame percentile improved while the client did ~8× the streaming
work.** That is the whole thesis: the streaming tax left the frame. `busy`
rising 1.0 → 1.4 ms is the absorption that CANNOT follow the jobs off-thread
(store bookkeeping, `write_buffer` — both pinned to the thread owning the
device), and it is a fifth of what the jobs were costing.

**Per-job cost is ordinary, and an earlier scare was a misreading.** A run
taken while the pool was feed-starved reported gen avg 5.85 ms and looked
like a 5× wasm penalty. It was the menu BACKDROP world's terrain — cave
noise makes column cost bimodal (PERF-AUDIT §2), and the earlier 1.17 ms web
figure was taken there too, not at the seed-42 spawn. At the same pose it is
1.29 ms in a worker against 0.83–1.01 ms native, i.e. wasm costs ~1.4×.
Workers also beat the main thread per job (1.29 vs 2.29 ms) because an
inline job's wall clock includes competing with the render loop.

**Read the `workers=0` arm as floor-only, not as the shipped client.**
`?bench` disables the adaptive allowance (`adaptive_effective`), so that arm
runs the fixed floor — and the tell is in the table: 451 mesh/s is
`INLINE_MESH_CAP` (4) × 120 fps almost exactly. A user on today's adaptive
inline path sits somewhere better than this. The shipped-vs-shipped number
needs overlay readings on a NON-bench run, which is still owed.

Two bugs found by taking this measurement at all, both worth remembering:

- **The pool was feed-rate starved at first**, capping throughput at
  `DEPTH × workers × fps` = 480 jobs/s with the pool 9% utilized and 1124
  columns queued. `top_up_workers` refills once per `maintain`, so a worker
  that empties its queue idles until the next frame. This is the SAME
  starvation the second pass found natively with `bounded(workers*2)`, and
  the same fix: depth 2 → 16.
- **`?bench` opened the menu**, because `EngineConfig::web()` starts with
  `menu: true` while desktop starts menu-less. The flight still ran (it
  returns before the interactive half), so this cost comparability and
  appearance rather than correctness — but "capture paths must not sit on a
  menu screen" is a documented rule and the web bench was breaking it.

## 2026-08-14 — what the web's inline executor actually costs

Groundwork for the browser's Web Worker chunk pool. `--workers 0` IS the web's
executor (`Pipeline::build` forces it on wasm), so a native bracket measures what
that platform gives up — modulo one caveat that turns out to matter.

M2 Max, 120 Hz vsync, `--bench --display 0 --fly 1 --frames 4800`, seed 42, draw 20.
`--fly 1` freezes the camera after one frame, so this is the initial fill with no
travel demand. Over-12.5 ms counts are out of 4739 scored frames.

| workers | fill | avg | p95 | p99 | over-12.5 ms |
|---|---|---|---|---|---|
| 0 (inline) | **113.4 s** | 8.47 | 9.64 | 16.52 | 687 (of 21939) |
| 1 | 23.4 s | 8.34 | 9.00 | 10.04 | 9 |
| 2 | 9.8–10.1 s | 8.34 | 8.73 | 9.21 | 5 |
| 4 | 3.6–3.7 s | 8.35 | 8.90 | 9.38–11.41 | 19–41 |

The inline row needed a 22000-frame run to finish at all; at 4800 frames it never
completed the fill, having managed 425 of the 1245 gen jobs in 40 s. On the
travelling flight (`--bench-speed 30`, 15 s) it is 6× fewer gen jobs and 13× fewer
mesh jobs than 4 workers, and never fills.

**Read the inline row as a lower bound, not as the shipped web client.** `--bench`
disables the adaptive allowance for determinism (`adaptive_effective`), so
`--workers 0` there runs the floor batch ONLY — one `Generate`, or
`INLINE_MESH_CAP` = 4 meshes, per frame. The shipped web build gets an allowance
equal to its whole budget, observed at 5.6 ms while streaming (see the 2026-08-06
web readings) ≈ 0.67 core-seconds/s, which against ~5.4 s of wasm job work puts its
real fill in the 8–10 s range — i.e. roughly where 2 workers land, not where 113 s
does. **On a machine with headroom the adaptive budget already buys most of what a
worker pool would.** The case for the pool is the hardware WITHOUT headroom, where
the controller collapses to its 3.0 ms floor while job costs rise 3–4×, plus the
floor batch that no controller can shed at all (PERF-AUDIT §1).

**A retraction.** A first pass of this bracket, taken on a thermally loaded machine
right after a release build, read 340 over-12.5 ms frames at 4 workers against 30 at
2, and was briefly used to argue a hard pacing knee at 2. Re-measured cold, twice, it
is 19–41 against 5 — the direction survives (absorbing results is pinned to the
thread that owns the device, so a faster pool spends more main thread per frame, not
less) but the cliff does not. Natively the 4-worker trade is still worth it. Two
remains the WEB default on grounds this bracket cannot measure: per-worker wasm
linear memory on low-RAM devices, and a `upload_bytes_max` of 768 KiB a frame against
native's 8 MiB, which must move the knee earlier there. Re-measure on web before
raising it. Lesson for the next bracket: pin the thermal state, not just `--display`.

## 2026-08-12 — the toroidal wrap was 25% of the frame

Same machine and pose as the attribution entry below: M2 Max, 5.43 MP (2992×1814),
`--bench --no-vsync --frames 1200 --fixed-dt 0.0166667`, seed 42, tod 45, draw 20.
Interleaved, 3 reps per arm with the first discarded — spread **0.02 ms**.

| | avg ms | fps | p95 | p99 | over-12.5ms |
|---|---|---|---|---|---|
| baseline | 11.75 | 85.1 | 16.33 | 18.53 | 458 |
| **after** | **8.83** | **113.3** | **12.35** | **15.09** | **51** |

**−2.92 ms, −24.8%, and 89% of the over-budget frames gone**, for four lines of WGSL
that change no arithmetic result. Capture is pixel-identical: mean **0.00002**/255,
max 1, 0.0024% of pixels, against a 0.11% capture-noise floor.

**It was address math, not shading.** `gi_cell` and `gi_coarse_empty` wrapped the
toroidal window with the textbook positive-modulo `((c % s) + s) % s`. On a GPU with no
integer divide unit each `%` is an emulated sequence, and the marches ran the pair twice
per iteration — once for the coarse brick, once for the fine cell — over two wrapping
axes. **Eight emulated divisions stood between every DDA step and its texture load**, in
the loop the stub table below attributes 40% of the frame to. Two changes, each measured
on its own:

1. **One modulo per axis** (`gi_wrap2`) — the second only fixes a negative first result,
   which a compare and an add do for free. 11.75 → ~9.6.
2. **One wrap for both levels of the skip** — the window side is a multiple of 8, so
   `wrap(c, side) >> 3` IS `wrap(c >> 3, side / 8)`, and the brick address is three
   shifts off the fine one. ~9.6 → 8.83.

Attribution first, which is what pointed here (baseline 14.84 on a noisier machine, so
read the shares, not the absolutes): `giflat` 6.48 ms / 44% of the frame, `gigather`
6.22 / 42%, `gisun` 3.37 / 23%, `giatrous` 1.21, `noscene` 0.93, `nowater` 0.56. The
final gather is essentially the whole resolve, and the gather is the DDA.

### The walking soak — the win widens, the ratchet does not go away

`--bench-speed 4 --frames 13500` (225 world-seconds, ~900 blocks), compared at the same
POINT ON THE WALK (cumulative frames), not the same wall-clock bucket — the faster build
is further along at any given second and the naive comparison flatters it.

| ~frame | 1000 | 3000 | 5000 | 7000 | 9000 | 11000 | 13000 |
|---|---|---|---|---|---|---|---|
| baseline ms | 10.57 | 10.96 | 17.15 | 15.96 | 14.99 | 15.08 | 15.15 |
| after ms | 6.10 | 7.28 | 10.26 | 10.49 | 8.69 | 8.87 | 9.53 |
| gain | −42% | −34% | −40% | −34% | −42% | −41% | −37% |

Whole-run: **14.58 → 8.84 ms**, hitches 216 → 1. The gain is *larger* at walking pace
than on the flyover, because the slow path is more DDA-bound and less streaming-bound.

**The session ratchet is scaled, not removed.** Baseline climbs 10.6 → 15.1 (+43%)
across the walk; the fixed build climbs 6.1 → 9.5 (+56%). Both degrade by about the same
FACTOR, which is the expected shape if the ratchet is "more DDA iterations per ray" and
this change is a constant factor on the cost of one iteration. It confirms the 2026-08-12
drill finding from the other direction: the decline is real geometry, and the lever on it
is per-iteration cost.

**Nothing accumulates on the CPU.** Across the whole 225-second walk, `busy_ms` moves
2.66 → 3.33 (tracking chunk count, identical on both arms), `gpu_bytes` 208 → 261 MB
(mesh density, identical on both), `emitters` 0 throughout, and `sky_y` 183 → 239 exactly
as the drill predicted. No unbounded container, no leak.

### Measured and rejected — do not retry

- **Cache the last brick index** and skip the coarse load while the march stays inside
  it. The redundancy is real (a dense brick is re-asked up to 8 times), but the load is a
  hit in a 51 KiB texture and the extra live register plus the divergent branch cost more:
  **12.02 → 14.03 ms.** Pinned in a comment at `gi_coarse_empty_w`.
- **Replace `gi_ray_skip_brick`'s three float divides** with multiplies by
  `max(abs(dir), 1e-6)` (which is `1/inv` by construction). No measurable change, and it
  puts an ulp of slop into a function whose comment records that the position form was
  measurably wrong. Reverted.
- **The same wrap fix in `gi_probe.wgsl`.** Neutral within noise — the probe compute is
  ~0.9 ms of the frame, not 6. Kept anyway, because the two steppers are pinned twins and
  must not drift.

### Caveat on the machine

Three soak pairs were thrown away to background macOS daemons (`duetexpertd`,
`speechmaintenanced` pegging two cores; load average 18). The tell is `gen jobs avg`
rising 0.85 → 2.6 → 4.1 ms and a `worst` in the hundreds of ms — the GPU numbers move
with it. Check `uptime` before trusting a bench, and note that the clean baseline here
(11.75) reproduces the 11.79 recorded in the attribution entry below, which is what
confirms the machine was in the documented state.

## 2026-08-12 — render scale, GI scale, and which one to actually spend

Same machine and pose as the attribution entry below: 5.43 MP (2992×1814), `--bench
--no-vsync --frames 1200 --fixed-dt 0.0166667`, seed 42, tod 45, draw 20. Interleaved,
2 reps — spread ≤0.09 ms.

| render scale | GI divisor | avg ms | vs native | image Δ vs native |
|---|---|---|---|---|
| 1.0 | 2 | 12.50 | — | — |
| 0.85 | 2 | 10.22 | −18% | — |
| 0.7 | 2 | 8.74 | **−30%** | mean **18.61**/255 |
| **1.0** | **3** | **8.72** | **−30%** | mean **0.11**/255, max 76, 4.7% of pixels |
| 0.7 | 3 | 8.33 | −33% | — |

**The two buy the same 30%, and the GI notch costs ~170× less picture.** That is not
intuition — the plan had render scale as the headline lever and GI-ratio decoupling
parked in reserve as "a genuine quality trade". It is the other way round, because the
à-trous filter and the bilateral upsample already reconstruct that signal, while a render
scale drop resamples everything including the geometry edges this engine is made of.

So the interactive **high-DPI default is native resolution with lighting detail at /3**
(`settings::density_defaults`), not a reduced render scale. Render scale remains a slider
for anyone who needs more than one notch's worth.

Applied once at the first `set_scale_factor` — the first moment anything knows the backing
scale — and only when the player has expressed no preference (no flag, nothing saved).
Every capture path leaves `gfx_defaults_auto` false, so bench/screenshot/drill stay on the
shipped values and no recorded baseline moves. Verified on a throwaway profile: logs
`display scale 2x: render scale 100%, lighting detail /3` and writes no settings file, so
the derived default follows the display instead of freezing into a saved preference.

Parity: at render 1.0 / GI 2 the new plumbing is mean **0.0013**/255, max 1 against the
pre-scale build — the `gi_div` uniform reproduces `chunk.wgsl`'s old hard-coded `/ 2`
exactly.

**Cost of the frame restructure itself** (water and the viewmodel moved onto a
render-resolution `post` target, one upscale pass to the surface), interleaved ×2 at
render scale 1.0: **12.03 → 12.26 ms, +0.235 ms (+2.0%)** — the extra native-res
fullscreen pass. It inverts the moment either scale drops below native.

**Trap worth keeping:** `blit_pipeline` is built with `pipeline_builder::build`, which
attaches a depth-stencil state, and a pipeline carrying one may only run in a pass that
HAS a depth attachment. It worked for years inside the water pass, which has one. Reusing
it in a standalone fullscreen pass invalidates the encoder — and an invalid pass poisons
the whole `CommandEncoder`, so `finish()` yields a command buffer where every pass is
dropped. The symptom is a black window with no HUD and all GPU timestamps reading 0.00,
which looks nothing like a bad blit. `upscale_pipeline` is the depthless twin.
`RUST_LOG=warn` surfaces it as "Encoder is invalid"; filtering logs to `wgpu` targets
hides it, because the handler logs under `digblock_platform::app`.

## 2026-08-12 — GI attribution (`DIG_STUB` extended), and the window-size trap

M2 Max, **5.43 MP (2992×1814)** — `--size 1728x1117` clamps to the usable screen, so this
is effectively fullscreen and the same framebuffer as `shots/f8-64.png`. `--bench
--no-vsync --frames 1500 --fixed-dt 0.0166667`, seed 42, tod 45, draw 20. Rep 1 of an
interleaved cycle (see the caveat at the end — this is one rep, not a settled floor).

`DIG_STUB` covered water only; the GI resolve — the frame's most expensive pass — had
never had a single term attributed. Added `gisun gigather ginee giflat giatrous` (resolve,
riding its own `jitter.w` since it binds no frame UBO) and `noscene noraster` (raster).

| `DIG_STUB` | avg ms | term cost | share of frame |
|---|---|---|---|
| *(none)* | 11.79 | — | — |
| `giflat` (whole resolve) | 4.70 | **7.09** | **60%** |
| `gigather` (96-iter final gather) | 5.92 | **5.87** | 50% |
| `gisun` (320-iter sun ray) | 9.02 | 2.77 | 23% |
| `ginee` | 11.79 | 0.00 | none — no emitters in a vanilla world, correct |
| `noscene` (scene chunk raster) | 11.85 | **~0** | — |
| `noraster` (all chunk geometry) | 7.13 | 4.66 | confounded, upper bound only |

**The GI resolve is 60% of the frame, and the final gather is a bigger term than the sun
ray** — the opposite of what the iteration counts (96 vs 320) suggest, because the sun
ray's sky-ceiling early-out fires on most upward rays while the gather marches 32 blocks
of real geometry. `noscene ≈ baseline` says the scene-pass chunk raster is free on this
TBDR: draw calls and triangles are a CPU-encode cost here, not a GPU one.

**The window-size trap, which is the finding that outlives the table.** The identical
bracket at the DEFAULT bench window measures *backwards* — `gisun` 7.19 vs a 6.28
baseline, `giflat` 6.88, every stub apparently *adding* time. That frame is not GPU-bound,
so removing GPU work changes nothing and run-to-run variance is the whole signal. Any GI
measurement taken at the default window size is worthless; use the real one.

Two stubs are confounded by construction and are documented as such in `renderer::stub`:

- `noraster` empties the depth buffer, so every resolve pixel takes the sky early-out and
  the most expensive pass goes cheap too. Never subtract it from `noscene` to get the
  prepass — there is deliberately no prepass-only stub.
- `giatrous` measured 12.19, *slower* than baseline, because skipping the filter also
  starves the history feedback into `[1]` and pushes more pixels down the prime path every
  frame. It is not the filter's cost. (The filter measured 0.14 ms when it landed.)

**`gpu_gi_denoise_ms` changed meaning.** Both à-trous iterations wrote query pair 12/13, so
the second silently overwrote the first and the column reported the stride-2 pass alone.
Each iteration now has its own pair and the column sums them: it roughly doubles (5.65 →
10.66 on the same scene). Pre-2026-08-12 CSVs read about half. `gpu_depth_blit_ms` and
`gpu_viewmodel_ms` are new — both passes ran untimed since they were written; the depth
blit is ~0.26 ms.

### The session ratchet — walking-speed soak

`--bench-speed B` (default **30**, every prior baseline's value) makes the flyover's speed
a flag. 30 blocks/s is 7.5× `WALK_SPEED` and a different *streaming* regime, not merely a
faster one: at that pace the GI upload budget cannot keep up. `--walk` is not a substitute
— its one-block oscillation never brings new terrain into the window.

`--bench-speed 4 --frames 13500` (225 world-seconds, ~900 blocks) at 5.43 MP:

| t_s | 5 | 30 | 60 | 70 | 135 | 155 |
|---|---|---|---|---|---|---|
| fps | 129.5 | 115.0 | 79.5 | 64.2 | 82.1 | 91.4 |
| `sky_y` | 183 | 183 | 199 | 223 | 239 | 239 |

`sky_y` is monotone and never falls, which looked like a ratchet: the CPU coarse mirror it
is scanned from is never cleared on eviction, so in principle it climbs toward the tallest
terrain seen anywhere in the *session* rather than anywhere in the *window*, and every
non-descending ray in the resolve breaks on it.

### That hypothesis is REFUTED — do not spend time on it again

`--bench --fly 9000 --frames 12000 --gi-drill-at 10500 --bench-speed 4`: camera frozen at
frame 9000, the drill (clear the mirror + emitter map, `gi_redirty_all`) fires at 10500,
against an identical control with no drill. Same pose, same thermal state, same frame
indices — the only difference is the GI's derived age.

| frozen-pose tail | fps | avg ms | `sky_y` |
|---|---|---|---|
| A, drill at 10500 | 83.8 | 11.93 | 223 |
| B, control | 83.9 | 11.93 | 223 |

Whole-run avg **11.57 vs 11.56**. `sky_y` was cleared from 223 and **rebuilt to 223**.
Clearing the derived state and rebuilding it from what is actually resident changes
nothing, because the state was already correct — 223 is the genuine ceiling of the
resident window, not a stale high-water mark. `sky_y` rising 183 → 239 across the soak is
real terrain entering the window as you walk.

So the reported "120 fps at spawn, 50 after wandering, never recovers" is **not stale GI
bookkeeping**. It is the true steady-state cost of a fully streamed world at full
resolution, and the resolve owns 60% of it. The `GiDraw::emitters` map is still an
unbounded leak worth fixing on its own merits (nothing prunes it on eviction, and
`flush_lights` sums it every frame before its early-out), but it is latent: only `Torch`
emits and worldgen places none, so a vanilla world measures `emitters 0` throughout.

**Bonus: this pair is the repeat floor.** Two full runs differing by 0.01 ms whole-run avg
puts the harness noise well under any term in the attribution table above, so those
single-rep numbers stand.

## 2026-08-11 — water attribution (`DIG_STUB`), R21

M2 Max, 5.4 MP (`--size 1496x907` on a 2× display — verify PIXEL dims, `--size` is
logical), `--draw-dist 32 --seed 42 --tod 45 --no-vsync --frames 1500`, settled interval
of the metrics log. **Per-pass GPU counters are not used here and should not be:** on this
TBDR the passes in `shots/f8-64.png` sum to 46.4 ms inside a 19.68 ms frame, because the
tiling stage of one pass overlaps the fragment stage of the last. The only honest number
is whole-frame time with one term removed at a time.

Pose: submerged, looking up at the surface — `--pos -280 131 -232 --yaw 150 --pitch 30`.
Chosen because it reproduces the user's report ("just as slow below the surface"), which
no fragment-count hypothesis survives. Same-build repeat floor **0.02 ms**.

| `DIG_STUB` | avg ms | saved | share of water |
|---|---|---|---|
| *(none)* | 14.85 | — | — |
| `wsun` | 10.93 | **3.91** | 55% |
| `wssr` | 13.16 | 1.68 | 23% |
| `wvert` | 13.93 | 0.91 | 13% |
| `wripple` | 14.77 | 0.07 | — |
| `wrefract` | 14.79 | 0.05 | — |
| `spray` | 14.71 | 0.13 | — |
| `wflat` | 8.52 | 6.32 | 88% |
| `nowater` | 7.69 | **7.15** | 100% |

Water was 48% of the frame; without it the pose runs 130 fps. Both dominant terms were
ray marches whose answers were multiplied by ~0 (GI-DESIGN.md R21). After gating them on
their own contribution:

| pose | water before | water after | frame before → after |
|---|---|---|---|
| submerged, looking up | 7.15 ms | **1.40** | 14.85 → 9.09 (67 → 110 fps) |
| submerged, flat | 5.22 | **1.20** | 16.34 → 12.35 (61 → 81) |
| above water, terraced river + cascade | 2.45 | **1.60** | 13.65 → 12.74 (73 → 79) |

Residual at the submerged pose: `wvert` 0.83 ms (now the largest water term), rest of the
fragment ~0.6, the two marches ~0.05 each.

**Harness note that outlives these numbers:** cross-build image parity needs
`--fixed-dt`. Everything animated rides dt, so two builds of different speed reach
`--frames N` in different world states; at DIG_RIVER that put a *same-change* diff at mean
6.9/255 against a 0.69 same-build floor, entirely in the ripple bands. With `--fixed-dt
0.0166667` the same-build floor is 0.0001 (max 1) and the real parity reads ≤0.0023.

## 2026-08-10 — Stage-B voxel light (LIGHT-DESIGN.md)

A/B on the seed-42 flight, same machine/session, baseline = pre-light main
(27f9a2a):

| metric | baseline | light (final) | note |
|---|---|---|---|
| fill | 4.29 s | 3.71 s | run-to-run variance band; not claimed as a win |
| avg | 8.43 ms | 8.41 ms | flat |
| p95 / p99 | 9.38 / 9.75 | 10.17 / 12.15 | the honest cost — fill-time skylight settling |
| over-12.5ms | 10 | 9 | recovered (see below) |
| hitches | 0 | 0 | |
| mesh jobs | 76 444 | 85 923 | +12%: chunks re-mesh once with settled light |

Two tuning findings, both now load-bearing:

- **`LIGHT_BUDGET` 16k → 4k per `step_light` site.** A pop costs ~6 hash-map
  neighbor reads; at 16k×2 per maintain a streaming backlog spent multiple
  milliseconds inside `maintain` and p99 went to 14.0 ms with 54 frames over
  12.5 ms. 4k×2 bounds the worst maintain near a millisecond; settling is
  later, never wrong (dirty-gating remeshes whatever settles late).
- **Dirty drains only when the field is SETTLED** (`pending() == 0`), not per
  budget slice. Per-slice draining re-bumped the same chunks every maintain
  while a backlog settled — +21% mesh jobs on the bench. One drain per settle
  = one light remesh per chunk. Test-side corollary: `quiesce` means
  chunk-quiet, not light-quiet — pipeline light tests must `settle_light`
  before asserting, or they race the budget.

Remaining p99 gap (~+2.4 ms) is fill-window skylight arrival work (descend
scans + the settle drain's remesh burst) under the vsync headroom; steady
flight shows `light q 0` every report window and zero hitches.

Acting on the measurement pass below. The drain now refuses to step past a
**hole** in the input tick sequence for up to `INPUT_HOLD_MAX` ticks, so an
input reordered by jitter is still admitted when it lands instead of arriving
under the high-water mark and dying there. `EDIT_POS_TOLERANCE` and
`dig_grace` are deliberately **not** made depth-aware — see the rule on
`INPUT_HOLD_MAX` in `protocol.rs`.

The bench grew a second link table for this, because the constants a hold
spends are edit-path constants and the sweep only ever walked.

### Before (`INPUT_HOLD_MAX = 0`) → after (`= 4`)

| link | in-deliv | late/s | rtt mean | repl/s | corr mean | corr max | depth | orig max |
|---|---|---|---|---|---|---|---|---|
| lan | 100.0 → **100.0** | 0.0 → 0.0 | 26.0 → 26.0 | 0.00 → 0.00 | 0.000 → 0.000 | 0.00 → 0.00 | 1.5 → 1.5 | 0.000 → 0.000 |
| wifi | 99.9 → **99.9** | 0.0 → 0.0 | 48.1 → 48.1 | 2.70 → 2.70 | 0.069 → 0.069 | 0.13 → 0.13 | 1.5 → 1.5 | 0.067 → 0.067 |
| cellular | 98.9 → **99.8** | 0.6 → 0.0 | 142.1 → 142.8 | 3.85 → 3.85 | 0.078 → 0.078 | 0.20 → 0.20 | 1.7 → 1.8 | 0.266 → 0.267 |
| bad-4g | 91.8 → **98.0** | 8.2 → 1.8 | 299.5 → 310.4 | 3.85 → 3.65 | 0.093 → 0.090 | 0.33 → 0.33 | 1.9 → 2.5 | 0.599 → **0.399** |
| awful | 73.0 → **89.5** | 26.3 → 8.9 | 430.2 → 474.3 | 5.40 → 4.65 | 0.273 → 0.220 | 0.73 → **1.07** | 1.7 → 4.6 | 1.021 → **0.867** |

**The lan and wifi rows are unchanged to the digit.** Not approximately —
identically, every column. A link with no reordering has no holes, so the
branch never fires. That is the whole cost argument: almost every player pays
exactly nothing for this.

**A hold does not spend `EDIT_POS_TOLERANCE` — it saves it.** This was the
prediction the sweep refuted, and it is the most useful thing it found. The
arithmetic said a deeper buffer must push the claimed edit origin further
from the server's simulated position, at 0.067 blocks per tick of walk. The
measurement says `orig max` *falls*: 0.60 → 0.40 at 300 ms, 1.02 → 0.87 at
500 ms. The reason is that the alternative to holding is coasting on stale
intent, and a coast diverges faster than a delay does. The tolerance headroom
was never the binding constraint at this cap.

**What it costs is latency, and only on links that already have some**:
+0.7 ms at 120 ms, +11 ms at 300 ms, +44 ms at 500 ms. Client prediction
hides all of it for movement; it is visible only where the server's current
position is compared against the client's, which is the `orig` column above —
and that got better, not worse.

### Honest debits

`corr max` at 500 ms goes the wrong way: 0.73 → 1.07 blocks. It is one
deterministic event (identical at caps 2, 4, 6 and 8), and it is the tail of
the mechanism — when a hole never fills, the server coasts the full cap and
then skips, which is a single larger divergence than skipping immediately.
The body of the distribution improves: `corr mean` −19% and `repl/s` −14%, so
total corrective motion falls from 1.47 to 1.02 blocks/s. Fewer, smaller
corrections with one rarer, bigger one — worth it, but not the unambiguous
win the acceptance criterion assumed.

`dig ok/att` at 500 ms reads 7/8 → 6/8. **This column cannot rank the cap**:
stone is 90 ticks, so a 20 s row gets 8 attempts and the sweep bounces around
4–8 with no trend. `dig short` is zero on every row at every cap, so
`dig_grace` is not what is failing — the abandonments are
`DIG_OFF_TARGET_RESET`, the server's `look` lagging far enough at 500 ms that
a hold exceeds 30 consecutive off-target ticks. That is the known
aim-has-no-adoption-envelope problem, not a regression from this change, but
a longer-window dig measurement would be needed to say whether a hold makes
it meaningfully worse.

### The sweep the cap was chosen from

`in-deliv` by cap:

| cap | 0 | 2 | 4 | 6 | 8 | 12 |
|---|---|---|---|---|---|---|
| cellular | 98.9 | 99.8 | 99.8 | 99.8 | 99.8 | 99.8 |
| bad-4g | 91.8 | 96.5 | **98.0** | 98.2 | 98.2 | 98.2 |
| awful | 73.0 | 82.5 | **89.5** | 93.0 | 94.3 | 93.7 |

and what the higher caps cost, on the `awful` row:

| cap | 0 | 2 | 4 | 6 | 8 | 12 |
|---|---|---|---|---|---|---|
| rtt mean | 430 | 455 | 474 | 484 | 487 | 488 |
| repl/s | 5.40 | 4.45 | **4.65** | 5.10 | 5.95 | 6.25 |
| orig max | 1.021 | 0.799 | **0.867** | 1.132 | 1.328 | 1.787 |
| tol-rej/s | 0 | 0 | 0 | 0 | 0 | **0.05** |

4 is the knee. bad-4g is done there — 6 and 8 add two tenths of a point
between them. awful keeps improving to 8, but buys it with a replay rate that
climbs back *above* the no-hold baseline, which is the symptom the change
exists to remove. And 12 is where the arithmetic finally bites: `orig max`
1.79 exceeds the 1.5 tolerance and honest edits start bouncing — the only
non-zero `tol-rej/s` anywhere in the sweep. The bench catches the failure mode
before a player would.

Both halves stay deterministic: the link and edit tables are bit-identical
run to run. The capacity table is wall-clock and is not — `tick us` swings
~30% with machine load — but a held tick does strictly less work than a
consumed one, so the capacity conclusion below is untouched.

## 2026-08-09 — netcode measurement pass (`just bench-net`)

**This supersedes the baseline below it**, which was partly measuring the
benchmark. Nothing in the engine changed: the harness was fixed, the server
input path was instrumented, and the numbers were re-taken. Recorded as a
second entry rather than an edit because the corrections are the finding.

### What was wrong with the first baseline

| column | defect | effect |
|---|---|---|
| `seen rtt` | the wire delivered once per tick, checked against the same `now` the send used, so `at = now + one_way` was never `<= now` | every leg cost a whole 16.67 ms tick regardless of the configured link |
| `snap/s`, `replay/s` | read from `StatsAcc`, which publishes a closed 1-second bucket | a phase of the run, not the run — wifi printed 20.7/s against a 20.0 nominal |
| `seen rtt`, `+-jit` | EWMA seeded during warmup and never reset | the 4 s startup transient rode into the reported value |
| `alloc/t` | counters bracketed the whole loop body while the timer bracketed only `tick_once` | the harness's own `Vec<SimEvent>` and per-client `vec![]` were being reported as the server's — ~74% of the number at n=64 |
| `tick us` | claims were `[0, -500, 0]`, so the `sane` chain short-circuited on `POS_TOLERANCE` | `probe_solid` → `overlaps_solid`, the most expensive call in the input path, was never run |

The link half now sub-steps the wire and the client 8× per server tick
(2.08 ms). The server still ticks at 60 Hz — a packet arriving mid-tick
genuinely waits for the next boundary, and that quantization is real. Every
link column is measured over the window rather than sampled from a rate
bucket, and clients claim a position predicted with the server's own physics
and re-seeded from server state each tick, so adoption really happens (the
harness asserts it rather than assuming it).

### Server capacity — real worldgen, everyone walking, 600 ticks/row

```
  n |    tick us: p50    p95    p99    max | snap B | kB/s/c | alloc/t   alloc | snap
  1 |    0.8    1.2    1.8   25.1 |   31.0 |    0.6 |     0.6      88K |  100%
  8 |    4.7    7.9   20.2  132.8 |  194.1 |    3.8 |     3.3    1796K |  100%
 24 |   12.9   44.5   65.9   84.5 |  567.0 |   11.1 |     9.3    7402K |  100%
 40 |   18.2   72.2  114.9  120.6 |  939.0 |   18.4 |    14.9   13969K |  100%
 64 |   29.1  138.0  199.8  208.5 |  937.3 |   18.4 |    23.1   19118K |  100%
```

p99 at 64 players is 200 us against a 16 667 us budget — 1.2%. The
conclusion from the first baseline survives: interest management, not tick
cost, is what bounds player count.

`alloc/t` at 64 players is **23.1, not the 88.6 first reported** — three
quarters of that figure was the harness allocating its own input events.
What remains is ~0.36 allocations per client per tick.

Adding the adoption path did **not** move `tick us` measurably. That is
itself a result: `overlaps_solid` is cheap next to residency and snapshot
assembly, so the per-client cost is dominated by interest management rather
than by validating movement.

### Link quality — real NetClient vs real Sim over a seeded impairing wire

Flat world on purpose: this half measures the protocol, and real worldgen
would put worker-thread completion times into the loop. Rows are
bit-identical run to run, so a difference is a difference in the netcode.

```
link     | cfg rtt  loss | drop | rtt mean  p95 | snap/s | repl/s | corr mean  max | max div | in-deliv | late/s want | depth
lan      |     4ms    0% |   0% |    26.0   35.4 |   20.0 |   0.00 |     0.000 0.00 |    0.07 |   100.0% |   0.0    0 |  1.5
wifi     |    30ms    1% |   1% |    48.1   70.8 |   19.8 |   2.70 |     0.069 0.13 |    0.33 |    99.9% |   0.0    0 |  1.5
cellular |   120ms    3% |   4% |   142.1  193.7 |   19.4 |   3.85 |     0.078 0.20 |    0.67 |    98.9% |   0.6    5 |  1.7
bad-4g   |   300ms    5% |   6% |   299.5  385.5 |   18.9 |   3.85 |     0.093 0.33 |    1.06 |    91.8% |   8.2   10 |  1.9
awful    |   500ms   10% |  12% |   430.2  554.2 |   17.8 |   5.40 |     0.273 0.73 |    1.81 |    73.0% |  26.3   18 |  1.7
```

**The `lan` row's RTT is 26.0 ms, not 41.9.** The old figure was ~16 ms of
harness quantization. 26.0 lands next to the 24.8 ms measured live against a
real QUIC server, which is the independent check that the model is now
honest. `seen rtt` still exceeds the wire on purpose — it is the input→ack
loop, so it carries the server's jitter buffer, the 60 Hz tick boundary and
the snapshot cadence, which is the latency a player feels. It does **not**
carry the send stride: `sent_at` is stamped at transmission
(`predict.rs:154`), so a tick waiting its turn on the wire is felt but never
counted. The first baseline claimed otherwise.

**`depth` is the finding.** The server's jitter buffer holds ~1.5 ticks on a
clean LAN and never reaches 2 on any row — including the calm ones. The
drain in `step_physics` takes at least one input whenever the buffer is
non-empty and has no way to take zero, so depth is a one-way ratchet: an
underrun destroys a tick of it permanently and nothing puts it back. The
first baseline said `JITTER_BUFFER_TICKS = 2` was "too small to absorb
±150 ms". That was the wrong diagnosis — the constant is not the mechanism,
and raising it would change nothing. Being able to *hold* would.

**`late/s` and `want` corroborate `in-deliv` from the server's own side.**
Late-lost inputs — arrived below the high-water mark having never been held,
so lost for good rather than being ordinary redundancy copies — are zero on
lan and wifi and climb to 26/s at 500 ms/10%. `want` says how deep the buffer
would have had to be to still be holding the worst of them: **10 ticks at
bad-4g and 18 at awful**, i.e. 167–300 ms. Any fix aimed at those rows has to
be willing to buy that much depth, and every tick of it is latency paid by
that player on every input — plus grace handed to anyone who fakes jitter,
since `dig_grace` and `EDIT_POS_TOLERANCE` are both sized against this lag.

Corrections stay small throughout (max 0.73 blocks at 500 ms/10%) and are
smoothed into the camera over ~200 ms, so the adoption envelope is holding
well past any link this game will realistically see. That is the reason the
delivery shortfall is a polish item rather than an emergency: a dropped input
is mostly overwritten by the next adopted claim. What the envelope does *not*
repair is `client.look`, which is written only from a consumed input and is
what `verify_digs` re-raycasts from every tick — so lost inputs degrade the
server's aim, not just its position.

## 2026-08-09 — netcode baseline (SUPERSEDED, see above)

First multiplayer numbers in this file; the netcode had none. Apple
Silicon (8P+4E), release build. Two halves, because they answer different
questions: server COST, and what a session FEELS like.

### Server capacity — real worldgen, everyone walking, 600 ticks/row

```
  n |    tick us: p50    p95    p99    max | snap B | kB/s/c | alloc/t   alloc | snap
  1 |    0.8    1.3    1.8   16.6 |   31.0 |    0.6 |     2.8     111K |  100%
  8 |    5.6    9.9   23.8   53.3 |  192.5 |    3.8 |    12.5    2444K |  100%
 24 |   11.0   40.2   57.8   70.3 |  561.8 |   11.0 |    34.6   10469K |  100%
 40 |   19.9   92.4  149.6  192.8 |  930.2 |   18.2 |    56.4   19385K |  100%
 64 |   31.5  199.1  271.6  342.0 |  930.3 |   18.2 |    88.6   26669K |  100%
```

p99 at 64 players is 272 us against a 16 667 us tick budget — 1.6%. The
sim thread is nowhere near its limit; interest management, not tick cost,
is what bounds player count. `snap B` flattening at 40 IS
`MAX_SNAPSHOT_PLAYERS` working. Per-client downstream tracks the v14
measurements (24 players: 562 B/snapshot and 11.0 kB/s here against 539 B
and 10.8 kB/s measured standalone) — an independent confirmation of that
change rather than a restatement of it.

### Link quality — real NetClient vs real Sim over a seeded impairing wire

Flat world on purpose: this half measures the protocol, and real
worldgen would put worker-thread completion times into the loop. Rows are
bit-identical run to run, so a difference is a difference in the netcode.

```
link     | cfg rtt  loss | seen rtt  +-jit | snap/s | replay/s | corr mean  max | max div | in-deliv
lan      |     4ms    0% |    41.9     8.5 |   19.7 |     0.00 |     0.000 0.00 |    0.13 |   100.0%
wifi     |    30ms    1% |    56.7    11.1 |   20.7 |     3.93 |     0.067 0.07 |    0.33 |   100.0%
cellular |   120ms    3% |   154.6    20.7 |   18.7 |     5.90 |     0.075 0.20 |    0.67 |    99.4%
bad-4g   |   300ms    5% |   306.7    38.4 |   19.7 |     5.90 |     0.132 0.79 |    1.51 |    91.7%
awful    |   500ms   10% |   415.0    65.0 |   17.7 |     4.92 |     0.243 0.80 |    1.78 |    74.4%
```

Two things worth reading carefully.

**`seen rtt` exceeds the wire RTT by ~38 ms, and should.** It is the
input→ack loop: send stride (0-33 ms) + the server's jitter buffer
(33 ms) + snapshot cadence (0-50 ms). That is the latency a player feels,
which is the number worth showing; a ping would report the 4 ms and
explain nothing. Confirmed live against a real QUIC server on loopback at
24.8 +- 8.5 ms.

**`in-deliv` falls off on the last two rows, and JITTER causes it, not
loss.** Heavy jitter reorders datagrams; `handle_inputs` skips anything at
or below `last_input_tick`, a high-water mark, so a tick arriving after a
newer one was consumed is discarded rather than applied out of order. At
`JITTER_BUFFER_TICKS` = 2 (33 ms) the buffer cannot absorb +-150 ms of
reordering. Deepening it would fix these rows and cost every other player
latency — a tradeoff to make deliberately and adaptively, on measured
jitter, not by raising a constant. Left as a finding.

Corrections stay small throughout (max 0.80 blocks at 500 ms/10%), and
they are smoothed into the camera over ~200 ms, so the adoption envelope
is holding up well past any link this game will realistically see.


All numbers: Apple Silicon (12-core: 8P+4E), release build, `just bench`
(seed 42, tod 45, 1800 frames, scripted flight, inputs off), ×3 runs.

## 2026-08-06 — frame-consistency pass (audit in `PERF-AUDIT.md`)

A full hot-path audit aimed at spike elimination, weighted to the web
build. Findings and the reasoning behind each fix live in
`PERF-AUDIT.md`; this entry records what changed and what was measured.

### Desktop A/B (vsync, seed 42, ×3 each, same machine and session)

Baseline is a `git worktree` at the pre-pass commit, benched interleaved
with the new build so machine state is shared.

| metric | before (×3) | after (×3) | |
|---|---|---|---|
| fill | 4.77 / 4.56 / 4.73 s | 4.02 / 4.20 / 4.23 s | **−11%** |
| avg | 8.34 / 8.35 / 8.35 | 8.36 / 8.36 / 8.36 | flat (vsync-bound) |
| p95 | 9.22 / 9.25 / 9.15 | 8.79 / 8.80 / 8.79 | **−4.7%** |
| p99 | 9.89 / 9.97 / 9.96 | 9.33 / 9.17 / 9.35 | **−6.3%** |
| worst | 17.0 / 17.0 / 16.7 | 24.7 / 17.2 / 17.3 | comparable |
| over-12.5ms | 2 / 3 / 3 | 4 / 5 / 5 | see below |
| gen job avg | 1.37 ms | 1.04 ms | **−24%** |
| mesh jobs | 81992 | 75676 | **−7.7%** |
| resident | 19920 | 20704 | +3.9% (hysteresis band) |

**p95 and p99 are the real signal**: they moved by more than their
run-to-run spread (p95 varies by 0.01 ms across the three new runs versus
a 0.43 ms gap to baseline), and fill improved consistently.

**`over-12.5ms` went slightly UP (2–3 → 4–5) and is reported as-is.** Two
extra frames out of 1799 is not a defensible regression signal at this
sample size, and the third-pass work above already established that these
counts match a static-scene control — they are compositor noise on this
machine, not app-caused. `over-8.3ms` (568 → 648) is likewise not
meaningful: avg sits at 8.36 against an 8.33 threshold, so a 0.02 ms shift
in the mean moves hundreds of frames across the line. Neither number
should be read as a tail regression, and neither should be read as a win.
If the tail matters, judge it on p99.

The web build is where this pass was aimed, and it has no scripted bench;
overlay readings are below.

### The headline: the adaptive stream budget was inert on web

`StreamBudget` derived busy time as `total_ms − acquire_ms`. That is
correct only where vsync blocks inside the swapchain acquire. On the web
`GPUCanvasContext.getCurrentTexture()` cannot block (it would deadlock the
JS main thread) and rAF paces the loop BETWEEN frames, so acquire read ~0
while `total_ms` was always a full display interval — headroom was
permanently negative and the budget pinned to its 3 ms floor forever, on
the one platform the feature exists for. At 120 Hz it was bistable
instead: full ceiling while work fit one 8.3 ms slot, hard collapse the
moment it spilled into two, i.e. a ~0.5–1 s sawtooth.

Verified by running the shipped controller directly (floor 3, ceiling 10,
600 frames, urgency 1.0):

| signal shape | frame ms | busy/acq | budget reached |
|---|---|---|---|
| native FIFO, 5 ms work | 16.7 | acq 11.7 | 10.00 (ceiling) |
| web rAF, 2 ms work | 16.7 | acq 0 | **3.00 (floor)** |
| web rAF, 8 ms work | 16.7 | acq 0 | **3.00 (floor)** |
| web rAF, true busy fed | 16.7 | busy 8 | 10.00 (ceiling) |

The existing test `near_miss_collapses_to_floor` had been asserting
exactly this shape (`on_frame(16.4, 0.4, …)` → floor) without anyone
noticing it *was* the web steady state.

Fix: occupancy is now MEASURED — `Game::frame_started` opens a span at
redraw-handler entry that `render` closes, minus the acquire wait, which
is right on both platforms. `web_signal_shape_still_grows` and
`high_refresh_web_tracks_work_not_quantized_delta` pin it.

### First web measurements (Chrome/WebGPU, Apple Silicon, 120Hz)

`?overlay&spikelog&seed=42`, backdrop world at the menu, draw 20. There is
still no scripted web bench — these are read off the runtime overlay — but
they are the first web frame numbers recorded here at all.

| state | fps | avg | p95 | p99 | worst | hitch | budget |
|---|---|---|---|---|---|---|---|
| streaming in | 119.9 | 8.34 | 12.30 | 12.80 | 13.30 | 0 | **5.6** |
| quiesced | 120.0 | 8.33 | 9.40 | 9.60 | 9.70 | 0 | 3.0 |

Two things to read off that table:

- **`acq 0.02 ms`** — the acquire wait really is ~0 on the web, which is
  the whole premise of the bug above. The old `total − acquire` derivation
  therefore reported ~16.7 ms of "busy" on a frame doing 3 ms of work.
- **`budget 5.6`** while streaming — the controller now grows past its
  3.0 floor, which it was structurally incapable of doing before. It
  returns to 3.0 once the world quiesces because urgency drops to zero
  with nothing pending; that part is by design (rim-only churn streams at
  the floor).

Per-job costs on this machine under wasm: gen 1.17 ms avg / 1.70 ms max,
mesh 0.11 ms avg / 0.40 ms max. The gen figure is far below the 12 ms
`CostRing` seed that the audit's worst-case reasoning was built on — the
bit-exact cave-noise work moved it — so job slicing (the one deferred
milestone) is now a smaller win than the audit projected.

### Measured wins

**Face re-dirty skips on the arrival path** (`pipeline.rs`). The
empty-mesh and queued-remesh skips applied only to diagonal neighbours;
they are equally sound for faces *on arrival*, because a chunk landing
turns all-air shell into real cells, so coverage only increases and a
neighbour that already meshed to nothing stays nothing. (Not true for
edits — digging removes coverage — so `set_cell` keeps its unconditional
bump.) Fill census, workers 4:

| seed / draw | mesh jobs before | after | mesh ms before | after |
|---|---|---|---|---|
| 42 / 8 | 3567 | 3328 (−6.7%) | 486 | 369 (−24%) |
| 42 / 12 | 10212 | 9181 (−10.1%) | 997 | 915 (−8.2%) |
| 1234.5678 / 12 | 7493 | 6456 (−13.8%) | 646 | 576 (−11%) |

**Mesher splash-ring skip**: full ocean-surface chunk 79.7 → 65.6 µs
(−18%), waterfall chunks unchanged, pinned byte-identical by
`water_heavy_chunk_mesh_is_pinned`. The per-cell tint hoist in the same
pass measured as **no change** (LLVM was already hoisting it) — kept for
explicitness, not for speed.

**Reliable frame encoding** 110 → 27 ns (4×) via in-place `postcard::to_io`
instead of an intermediate Vec; benefits the server equally.

### Structural changes without a single-number result

- `visibility_scan` was a full-world rescan on every chunk crossing —
  ~20k retain slots + ~27k hash probes + a full mesh-heap rebuild at draw
  20, running OUTSIDE the streaming deadline, i.e. an unbudgeted burst at
  walking cadence. Now a scanline interval diff of the entering/leaving
  crescents: O(r) instead of O(r²). Eviction gained one chunk of
  hysteresis (`KEEP_MARGIN`) so pacing across a single boundary stops
  regenerating the same crescent — costs ~10% more resident columns at
  draw 20. Pinned by `incremental_scan_holds_the_residency_invariant` and
  `boundary_pacing_does_not_regenerate`.
- The three streaming budgets (jobs / drain / uploads) summed to ~1.5×
  the nominal budget; they now consume one shared deadline, each keeping
  a minimum-progress floor.
- Uploads are bounded by BYTES as well as time (768 KiB on web) —
  `write_buffer` only records the copy, and Chrome's GPU process performs
  the staging transfer at submit, so the real cost landed after everything
  `upload_ms` could see.
- Eviction and the inbound net-event stream are both spread across frames
  now (join sync used to apply a server's whole journal in one frame).
- Persistence is sharded by 256×256-block region, so a save rewrites what
  changed rather than the world's entire edit history — see below.

### CSV schema changed

New columns: `dropped`, `refresh_ms`, `busy_ms`, `net_ms`, `sim_ms`,
`sav_ms`, `prep_ms`, `upl_q`, `evict_q`. As with the `gpu_water_ms`
insertion, **don't naively diff old and new CSVs**. `prep_ms` closes a
real attribution hole: remote sampling, name tags, UI rebuild and frame
uniforms were previously untimed, so a spike there appeared in
`total_ms` and in no phase at all. `DIG_SPIKELOG` is now an engine config
flag (`?spikelog` on web) rather than an env read that wasm always
answered None to.

## 2026-08-01 (seventh pass): water realism (lighting, falls, depth shading, spray, SSR)

Water gained fresnel + distance-widened specular + procedural sky
reflection on analytic ripple normals, two-phase texture advection,
per-vertex fall distance driving gravity stretch / aeration / streaks,
splash ring codes, and — via a render restructure — depth-based
shading: solids + sky now render to an offscreen scene color, the
solid depth is copied, and a separate water pass (blit composite +
prepass + color, new `gpu_water_ms` CSV column) samples both for
Beer-Lambert absorption, refraction, soft shores, and a desktop-gated
SSR march. Stateless spray billboards ride `MeshData.impacts`. The
platform-owned depth texture is gone (renderer-owned FrameTargets);
net GPU memory ≈ +1 full-screen color target.

Costs, ×3 runs at the shipping desktop config (SSR on):
gpu p50 main 1.78–1.90ms + water 0.58–0.89ms vs 2.28ms pre-change
single main pass — the split + blit + depth copy cost ≈ +0.1–0.4ms.
Frame pacing unchanged: avg 8.33–8.35, p95 9.3–9.5, hitches 1,
over-12.5ms 1–4 (sixth-pass profile). Mobile config (SSR off):
main 0.41–0.56 + water 0.45–0.49ms. SSR marches everywhere with a
distance-scaled stride (fixed 28 taps; far water takes coarse steps
that reach the mountains across a lake — an early 80-block cutoff
killed exactly those reflections); worst case (grazing ocean, ~60%
water coverage) ≈ +1.7ms, `--no-ssr` removes it.
Uncapped (`--no-vsync`): avg 2.77ms, 342 fps. Shader ALU additions
(ripples/fresnel/spec/reflection/advection, phases 1–2) measured
inside run noise. One caught regression: SSR ungated cost 1.70ms on
grazing ocean views — the distance gate more than halved it.

Reference shots: `shots/water-realism-shore.png` (soft shoreline,
absorption gradient), `shots/water-realism-sunset.png` (tod 75, yaw
−90: glitter path + sky reflection), `shots/water-realism-falls.png`
(coastal cascades: stretch, aeration, streaks, spray). Captures use
the new `--yaw`/`--pitch` flags (the spawn camera faces +z,
perpendicular to the sun's ±x azimuth plane). Note for future
pixel-diff verification: water shading is clock-animated and the clock
advances by real dt, so identical-flag runs differ on water pixels —
compare against a same-binary run-to-run noise floor, not byte
equality (this pass: ~37k px noise on water-heavy 1600×1200 views).
CSV schema change: `gpu_water_ms` inserted after `gpu_main_ms` — don't
naively diff old/new CSVs.

## 2026-07-31 (sixth pass): water system (levels, sloped surfaces, falls, sim, anim)

Water moved to Minecraft-style level blocks with its own two-word
vertex format + water.wgsl (solid path untouched), worldgen emits
WaterFall sheets at river pool steps (zero new noise — classify reuses
the neighbor fields it already had), and an edit-time fluid sim drains
128 queued cell updates per 0.25s world tick (idle = free). Terrain
retunes in between (ocean shelf + tanh continentalness + seafloor
detail, ridge^1.6 mountain profile) cost one tanh + one powf per
column. Costs vs the fifth pass: gen 1.03-1.06ms (flat), mesh avg
0.08 → 0.09ms, fill 4.66-4.81s (noise-level). One caught regression:
letting WaterState/emit_water_face inline into mesh_chunk's block loop
cost +0.03ms mesh avg on the dominant solid path — both are
#[inline(never)] now. Reference shots: `shots/water-falls.png` (seed
1234.5678 coastal cascade), `shots/sloped-water.png`.

## 2026-07-31 (fifth pass): visual depth pass (padded snapshot, AO, tint)

Mesh jobs moved from six border planes to a padded 18³ snapshot
(diagonal neighbors now visible), on top of which landed per-vertex AO
(bits 18-19), face-axis ambient (shader const), and per-block tint
jitter (bits 29-31, world-coord hash). Cost accounting, vsync fill,
step by step:

| step | fill | mesh jobs | mesh avg |
|------|------|-----------|----------|
| fourth-pass baseline | 3.97–3.98s | ~67k | — |
| padded snapshot | 3.39–3.63s | 66–67k | 0.07ms |
| + 26-way invalidation | 4.44–4.48s | 77.5k | 0.07ms |
| + AO + axis + tint | 4.75–4.76s | 77.7k | 0.08ms |

The snapshot refactor is a net win (workers stopped holding store Arcs;
verified bit-exact via a temporary FNV pin over 2×2 columns × 2 seeds).
The 26-way invalidation is the real cost of border-correct AO: a chunk's
arrival re-dirties diagonal neighbors whose shells it fills. Unbounded
that was 85k jobs / 5.3–6.3s fill; three provable skips (air chunks,
empty-mesh neighbors, queued-undispatched remeshes, all-air shared
cells) bought it down to +16% jobs. Net fill +20% over the fourth pass;
p95/p99 and hitch counts unchanged (9.5–9.6 / 10.7–11.1, 0–1 hitches).
AO itself is ~free at mesh time (12 shell reads per visible face) and
one mul in the shader. Reference shots: `shots/visual-depth-final.png`,
`shots/ao.png`, `shots/axis10.png` (low sun).

## 2026-07-31 (fourth pass): terrain realism cost check

The realism pass (domain warping, erosion field, altitude-weighted
detail, anisotropic ridge belts, slope/dither surface classification
with snow, pooled noise-band rivers, karst-anisotropic caves) roughly
doubles the 2D noise per column (~11 → ~21 `noise2`, plus a 1.27×
padded-grid factor for neighbor-height slope reads). Per-block cave
noise — the actual gen-cost driver — gained zero evaluations, so the
worldgen additions land inside noise:

| metric | before (same day) | after |
|--------|-------------------|-------|
| fill (vsync) | 3.98s ×2 | 3.97 / 3.98 / 3.98s |
| gen job avg | 1.05–1.06 ms | 1.07–1.08 ms (+2%) |
| fill (uncapped) | 2.02s | 2.07s |
| p95 / p99 (vsync) | 9.5 / 10.9 | 9.5 / 10.6–11.1 |
| over-12.5ms (vsync) | 1–2 | 1–5 |

Note these fill/gen numbers are NOT comparable to the third-pass tables
above — caves, trees, and the 256-block retune landed in between.
Column classification runs on an 18×18 padded field grid per chunk
stack (324 evals vs 1280 naive for the 5-sample per-column path); the
grid and per-column paths are pinned bit-identical by
`classification_paths_agree`. Skipped after consideration: the optional
vertical-shaft cave band — it would add a `noise2` per solid block on
the hottest per-block path for a subtle effect. Reference screenshots:
`shots/realism-final.png` (seed 42), `shots/realism-river.png` (seed
1234.5678, estuary + pooled river cascade).

## 2026-07-31 (third pass): frame pacing under fast streaming

The throughput pass made chunks stream in ~8× faster but visibly
jittered the render loop. Diagnosed with `DIG_SPIKELOG=1` (logs a
per-phase breakdown of every frame over the 120Hz budget) and a
vsync-paced bench run (Fifo present — pacing as actually played). The
bench summary now reports `over-8.3ms` (budget-fit) and `over-12.5ms`
(visibly dropped frame at 120Hz) counts.

**Bench default changed to vsync-paced.** An uncapped (~350fps) stream
sampled by a 120Hz compositor judders *by construction* — displayed
frame deltas vary regardless of how smooth the render loop is — so
watching an uncapped bench always looks bad. `just bench` is now
Fifo-paced (smooth, dropped frames measurable); use
`just bench --no-vsync` for uncapped raw-cost percentiles (the
second-pass tables below were captured that way and stay valid).

**Post-fix regression**: the first cut of the wide-map split flipped
`wide_front` mid-frame, after the frame UBO was written — so the flip
frame sampled the *new* map with the *old* VP, a dark far-shadow flash
at every 3.33s refresh. Map and VP now always change together (the
sampled index is captured before the slice encode; a completed map is
first sampled the next frame).

### What the spike log attributed

- **Streaming bursts**: `drain_results` capped only *meshed* results —
  `Generated` drains (stack insert + neighbor cascade) were unbounded,
  and the adaptive upload budget hit 64/frame, so boundary-crossing
  frames did scan + drain + up to 3.7ms of uploads + a co-spiking
  encode ≈ 8ms of CPU → missed vsync clusters.
- **Wide-cascade refresh**: every 3.33s the wide shadow map re-encoded
  the whole resident set (~2.6k draw calls, 3.8–8.2ms of CPU encode) in
  a single frame — a guaranteed rhythmic hitch.
- **Environmental stalls**: ~15ms acquire waits with zero app work
  (GPU ~1.3ms, all phases ≈ 0). A static-scene control run (no flight,
  no streaming, camera still) shows the same p99 ~15.7 / worst ~18 per
  window — compositor/OS noise on this machine, not app-caused.

### Fixes (all kept)

**1. Time-budgeted streaming** (`pipeline.rs`, `engine.rs`):
`STREAM_BUDGET_MS = 3` per frame. The first half bounds the result
drain (leftovers wait in the unbounded channel — worker throughput
unaffected); the rest bounds uploads via a `pending_uploads` carry-over
queue (min 4/frame so drains can't starve it; evicted chunks are
filtered out; fill time counts until the queue empties).

**2. Wide-cascade slicing** (`renderer.rs`, `texture.rs`): two wide
maps ping-ponged. A refresh snapshots the resident set and encodes 640
draws per frame into the back map (~4 frames per refresh), then flips;
the main pass keeps sampling the front map with the VP it was rendered
with (`wide_active_vp`) until the flip. Screenshot pixel-identical to
the pre-split reference (`shots/post-f64-terrain.png`).

### Results (vsync-paced flight — now the `just bench` default, ×3)

Streaming-attributable dropped frames (>12.5ms with mnt/upl activity)
fell from dozens per run to **2–4 per 1800 frames**; the wide-refresh
hitch is gone (slices encode ~1ms). Remaining `over-12.5ms` counts
(21–42/run) match the static-scene control — environmental. Fill at
120Hz: 0.78–0.81s.

Cost: uncapped fill 0.55 → **0.58–0.61s** (the budget defers work to
later frames — accepted for pacing); mesh jobs 13.6k → ~14.4k (drains
spread over more frames → slightly more cross-border remesh cascades).

### Addendum: the uncapped-run "surges", and nearest-first uploads

The surging jitter visible when *watching* the uncapped bench was
investigated against a suspected streaming bottleneck. Findings:

- A static-scene uncapped control (camera parked, zero streaming, zero
  uploads) shows the **identical stall fingerprint** as the full flight:
  43 frames >12.5ms in 1800, avg ~19ms, one every ~40 frames (~115ms
  cadence) — vs 43 / ~19.5ms / ~40 for the flight. Most flight stall
  frames do zero maintain/upload work. The stalls are compositor-side
  acquire waits on this machine, not the streaming path.
- The stalls are *visible as speed surges* because the bench flight
  advances one fixed 60Hz tick per frame (for run-to-run determinism):
  at uncapped fps, camera speed is proportional to instantaneous frame
  rate, so every stall freezes the flight ~20ms and every fps swing
  modulates its speed. Real gameplay integrates real dt and doesn't
  inherit this; the vsync-paced default bench doesn't either.

Kept from the investigation: **nearest-first uploads** (`engine.rs`).
The gen/mesh heaps were already distance-ordered, but the
`pending_uploads` carry-over was FIFO — on fast travel the column the
player is in/above could wait behind distant ring chunks. The queue is
now sorted nearest-first each frame (µs for its typical size), and
`UPLOAD_MIN` ≥ stack height guarantees the player's column uploads the
frame it drains; only the far ring is allowed to lag. Fill/pacing
unchanged (uncapped 0.58–0.61s, vsync 0.78s, drop counts at the
environmental floor); screenshot pixel-identical.

## 2026-07-31 (second pass): uncapped bench + streaming throughput

The bench now runs with **vsync off** (`AutoNoVsync`) and reports **fill
time** (first pipeline quiesce = initial world fill) plus run-level
percentiles (warmup excluded). Numbers below are NOT comparable to the
earlier vsync-bound table at the bottom.

### Uncapped baseline (pre-optimization)

| run | fill s | avg ms | p95 | p99 | worst | fps | gen jobs | mesh jobs |
|-----|--------|--------|-----|-----|-------|-----|----------|-----------|
| 1 | 4.62 | 2.49 | 6.32 | 19.75 | 27.2 | 380 | 5618 | 14811 |
| 2 | 4.78 | 2.63 | 7.48 | 19.28 | 25.1 | 362 | 5675 | 14893 |
| 3 | 4.79 | 2.68 | 7.69 | 19.55 | 24.2 | 356 | 5668 | 14939 |

Key insight the vsync-bound bench hid: **the worker pool was starved by
its feed rate, not by worker speed.** The job channel was
`bounded(workers*2)` = 8 and topped up once per frame, capping throughput
at ~12 jobs/frame; `requeue` fired every frame and the pipeline never
caught up with the flight (fill ≈ the whole run; chunks were evicted
before ever generating).

### Optimizations applied (each measured ×3)

**1. Job channel depth** (`pipeline.rs`): `bounded(workers*2)` →
`bounded(workers*16)`.

| metric | before | after |
|--------|--------|-------|
| fill | 4.6–4.8s | **0.59–0.68s (~7×)** |
| stale-dropped / run | ~560 | ~215 |
| requeues / run | ~1790 | ~220 |
| p99 / worst | 19.3–19.8 / 24–27 | unchanged |

avg_ms rose 2.5→3.0 because frames now carry real streaming work instead
of deferring it — strictly more useful work per second.

**2. Worker count scaling** (via `--workers`, after the channel fix):
fill 0.94s @2, 0.59s @4, 0.57s @6, 0.58s @8, 0.61s @10. Flat beyond 4 —
the desktop default stays `(n-2).clamp(2, 4)`; the pipeline is
drain/upload-bound past that, not worker-bound.

**3. Mesher** (`mesher.rs`): pre-sized `MeshData` Vecs (kills the
realloc-doubling chain), `border_plane` rewritten to contiguous
`copy_from_slice` for ±y/±z planes, incremental flat-index neighbor
probes. avg frame 3.01–3.09 → 2.73–2.85ms (~9%); mesh job max 1.5 →
0.3–0.6ms (realloc spikes gone).

**4. Column generation** (`worldgen.rs`, `pipeline.rs`): one gen job now
builds the whole vertical chunk stack from a single 16×16 heightmap
(noise work halved — was computed once per y-chunk), and `build_chunk`
fills in storage order (x contiguous) instead of striding 256B per
write. Gen jobs 6.7k → 3.4k at the same per-job cost; mesh jobs down ~7%
(stack siblings arrive together → fewer cross-border remesh cascades).
Also simplified the Java f32-widening leftovers to plain f64 — terrain
for a given seed diverges (allowed); reference shot
`shots/post-f64-terrain.png`.

**5. Upload budget cap** (`engine.rs`): `UPLOAD_BUDGET_MAX` 32 → 64.
fill 0.60 → 0.57s median, p99 19.1–19.9 → 17.8–18.7 consistently;
worst unchanged.

### End state (×3)

| run | fill s | avg ms | p95 | p99 | worst | gen jobs | mesh jobs |
|-----|--------|--------|-----|-----|-------|----------|-----------|
| 1 | 0.57 | 3.05 | 8.19 | 18.49 | 27.4 | 3354 | 13703 |
| 2 | 0.54 | 2.69 | 7.37 | 18.72 | 26.3 | 3362 | 13567 |
| 3 | 0.59 | 2.58 | 7.28 | 17.84 | 23.7 | 3360 | 13600 |

**Fill time 4.7s → 0.57s (~8×).** Frame percentiles unchanged-to-better
throughout.

### Where the time goes now (steady state, uncapped)

| phase | ms | notes |
|-------|-----|-------|
| acq | 2.0–2.5 | swapchain back-pressure — the run is **GPU-bound** (~1.8ms GPU/frame) |
| gpu shadow | ~1.1 | largest GPU pass |
| gpu main | 0.7–1.0 | |
| enc (CPU) | 0.64–0.82 | ~760 draw calls/frame; the lever is batching/instancing |
| upl | 0.12–0.15 | negligible |
| mnt | ~0.015 | negligible |
| gen job | 0.02 avg | negligible |
| mesh job | 0.04 avg | negligible |

**Skipped after measurement**: worker-clamp raise (flat beyond 4).
**Future work (architectural, not attempted)**: draw-call
batching/instancing for `enc` + `draws`; shadow-pass caster tightening
for `gpu shadow`.

---

## 2026-07-31 (first pass, historical): vsync-bound baseline

Captured before the bench went uncapped; kept for provenance. Display
120Hz Fifo, so total frame time pinned at 8.33ms and only phase columns
were judgeable.

| run | avg ms | p95 | p99 | worst | fps |
|-----|--------|-----|-----|-------|-----|
| 1 | 8.33 | 10.63 | 11.57 | 12.26 | 118.8 |
| 2 | 8.38 | 10.84 | 12.16 | 18.05 | 118.5 |
| 3 | 8.35 | 10.64 | 11.71 | 14.18 | 118.6 |

Optimizations from that pass (both kept): adaptive upload budget
(4→…→cap under saturation, decay when clear) — `sat` ~97% → ~10% of
streaming frames, startup hitch eliminated; water-sort scratch reuse in
the renderer (below noise but strictly less work). Skipped after
measurement: gen-job caching, GPU buffer pooling, stale-drop reduction —
all negligible at the time.

## 2026-08-02 — two-word vertex format (Phase 2 format break)

All geometry moved to two u32 words/vertex (half-block positions for
future stairs, 10-bit texture layer): solid vertex bytes doubled. Bench
(vsync, seed 42): fill 4.56s, avg 8.35ms p99 10.68ms, over-12.5ms 3,
hitches 1, GPU buffers ~87-103MB during fill. Within noise of the prior
baseline — the doubled solid upload bytes did not move fill time or
frame pacing measurably. Deterministic capture is byte-identical to the
single-word format.

## 2026-08-03 — worldgen v5 retune (baseline drift note)

Fill-time baselines above predate the v5 terrain retune (region
wavelengths 2–3× longer, flat steppes): the same seed now generates
different terrain, so fill numbers will drift. Re-baseline on the next
bench pass; frame-pacing methodology is unchanged.

## 2026-08-05 — twelfths vertex repack (sub-block shapes)

Positions moved to 8-bit twelfth-block units (word0) with explicit
sub-rect UVs + tint in word1; shaders decode via an integer whole/
remainder split so full-block geometry is exact under GPU fast-math
(pixel-compared against the pre-repack build: differences at the same-
build run-to-run noise floor, ~30 bytes of 7.7M). Bench (vsync, seed
42): fill 4.51s, avg 8.39ms p95 9.48 p99 10.46, over-12.5ms 6, hitches
0 — within noise of the two-word baseline. Mesh jobs read 0.10ms avg
vs the ancient 0.04ms table entry; that row predates several format
changes and per-call state masking in `MeshInput::get` — throughput is
still ~5400 jobs/s, far above streaming demand. Shaped blocks mesh via
baked quad templates (`emit_shaped`, outlined like the water path), so
the hot full-block loop is unchanged.
