# Bench baseline & hotspot analysis

## 2026-08-17 — a ray per light on players and confetti (R34): free where it matters, unpriceable where it might not be

The dither fix (GI-DESIGN R34) trades the resolve's one resampled shadow ray for one at each of the brightest
`GI_NEE_TAPS` emitters, on player pixels only. Release, M2 Max, `--seed 7 --fixed-dt 0.016 --size 960x540 --offscreen`.
Frame times off the swapchain have no present wait, so they compare only with each other, never with a baseline above.

**Pixels.** Every row is before-vs-after; the control is the same binary run twice, because this engine does not
reproduce itself exactly.

| scene (300 frames)                                               | moved | mean   | worst |
| ---------------------------------------------------------------- | ----- | ------ | ----- |
| terrain, no player (`--tod 90`)                                  | 0.19% | 0.0006 | 123   |
| ...its own control                                               | 0.31% | 0.0010 | 123   |
| `DIG_VAULT_DARK=1 DIG_SCREEN=torch DIG_PLAYERBOX=1` (`--tod 90`) | 0.00% | 0.0000 | 1     |
| `DIG_VAULT=1 DIG_PLAYERBOX=1` (`--tod 90`)                       | 0.00% | 0.0000 | 1     |
| `DIG_ORE=3 DIG_PLAYERBOX=torch DIG_SCREEN=torch --yaw 90`        | 1.17% | 0.0025 | 71    |

The two vault rows are the calibration guarantee as a measurement: one light in range and the two estimators agree to
the last bit. Terrain moved LESS than its own repeat, which is what "the resolve is untouched" looks like. The ore
chamber is the repro, and every moved pixel is on the body or its held item. Tap count in that same chamber, shipped
value against four times it: 1.41% of pixels, mean 0.0032, worst 22 — the dropped tail, and it is confined to the body.

**Frame time, `DIG_TORCHES=8 DIG_TORCHES_WALL=1 DIG_PLAYERBOX=dirt`, 4000 frames**, camera parked, `gpu_main_ms` from
the CSV rows after the scene quiesces (t ≥ 15 s):

| build  | samples                      |
| ------ | ---------------------------- |
| before | 5.24, 5.02, 5.03, 5.00       |
| after  | 5.50, 4.84, 4.73, 4.73, 4.82 |

**No separation** — eight emitters in range, so every tap is filled and marching, and the extra rays do not come out of
the spread. That is the bed that answers the shipped case: a body covers a small part of the frame and a room holds a
handful of torches.

**The ore chamber could not be priced and that is the finding.** Its `gpu_main_ms` ran 3.5–4.2 before and 4.1–5.4 after,
which looks like a real cost until you notice `avg_ms` climbing monotonically through every run, both builds, and
`DIG_STUB=ginee` (which removes the term entirely) landing inside the same range. The scene never reaches steady state:
seams keep streaming in, so the light list keeps growing and the resolve's own scan grows with it. **Do not use
`DIG_ORE` as a frame-time bed** — it is a picture bed. Pricing this properly wants a scene with a long, STABLE light
list (`DIG_TORCHES_FAR` pads the list without lighting a pixel, which is the shape of it) and a body filling a known
fraction of the frame; nobody has built that, and the courtyard result was enough to ship on.

**The same estimator on the confetti costs nothing measurable**, and the reason is the unit of work: it runs per VERTEX
on a few hundred vertices a burst, where the player term runs per fragment on tens of thousands.
`DIG_VAULT=1
DIG_CONFETTI=1`, 4000 frames, `gpu_water_ms` (the pass the billboards draw in) — before 0.964, 1.025,
1.211, 1.158; after 0.944, 0.962, 1.196, 1.229, with `avg_ms` matching row for row. Pixels: the vault burst moves 1.34%
of the frame (black paper to lit paper, worst channel saturated), and a noon capture over open terrain with a burst in
frame is unchanged at worst 2 — nothing in range, nothing summed, and the picture is the proof rather than the argument.

## 2026-08-15 — wgpu 26 → 30 is pixel-clean, and `--offscreen` is how it was measured

Four majors of wgpu at once (optional vertex slots and bind-group layouts, `multiview_mask`, `Option` depth fields,
`MipmapFilterMode`, a `CurrentSurfaceTexture` enum in place of a `Result`, `Queue::present`), plus glam 0.30 → 0.33
moving the projection onto `camera::rh::proj::directx`. Mechanical, but it touched every pipeline in the engine, so the
question was whether the picture moved.

Debug build, M2 Max, `--seed 1234 --tod 40 --exit --fixed-dt 0.016 --size 960x540`, diffed as raw RGBA bytes. Every row
is paired with a CONTROL — the same binary run twice — because this engine does not reproduce itself exactly and a
cross-version number means nothing without knowing its own floor.

**The settle length is the measurement.** A still of an accumulating renderer is only as comparable as its history is
converged, and `--frames N` is what buys that:

|             | control: same binary ×2         | wgpu 26 swapchain vs wgpu 30 `--offscreen` |
| ----------- | ------------------------------- | ------------------------------------------ |
| 60 frames   | —                               | 14.61% of bytes, max 165, mean 0.25        |
| 400 frames  | 0.26%, max 2, mean 0.0027       | 2.03%, max 3, mean 0.0203                  |
| 1200 frames | 0.26%, max 1, mean 0.0026       | —                                          |
| 2500 frames | **0.049%, max 1, mean 0.00049** | **0.059%, max 1, mean 0.00059**            |

**Converged, the two builds are the same picture.** At 2500 frames the cross-version diff is the control's own number —
0.059% against 0.049%, max 1 LSB either way. Four majors of wgpu and three of glam moved nothing.

Read the short runs as a warning, not a result. At 60 frames the A/B looked alarming and was measuring the spawn settle
(physics ticks ride the wall clock, so two builds of different speed arrive at `--frames N` in different world states —
this is the trap `--fixed-dt` exists for and does not fully close). At 400 it looked like a real ULP residual and
invited a plausible story about glam's `perspective` swapping a divide for a reciprocal multiply; another 2000 frames of
accumulation dissolved it. **Nothing was chased, because the number that would have justified chasing never survived a
longer settle.**

The A/B needed `--offscreen` to exist at all. wgpu 30's Metal backend refuses to hand a texture to an occluded window,
and both runs were taken with the screen locked; through the swapchain the wgpu-30 arm produced zero frames in 20
minutes at 97% CPU (which is also how the `SurfaceOccluded` → `IdleThenRetry` backoff got found). Frame times are NOT
comparable across that pair — no present wait off the swapchain — so nothing here supersedes a baseline; this entry is
about pixels only.

## 2026-08-14 — web chunk workers: the first scripted web bench

The first `?bench` numbers ever taken in a browser. Chrome/WebGPU, M2 Max, 120 Hz,
`?bench&fly=1&frames=1500&seed=42&mute`, draw 20, lighting detail at the density default for that display scale.
`?workers=` picks the arm.

|                         | `workers=0`                | `workers=2` (shipped)          | `workers=4`       |
| ----------------------- | -------------------------- | ------------------------------ | ----------------- |
| fill                    | **n/a — never, in 12.5 s** | **11.91 s**                    | **4.63 s**        |
| avg                     | 8.33                       | 8.33                           | 8.34              |
| p95                     | 10.60                      | **9.30**                       | 9.40              |
| p99                     | 11.10                      | **9.70**                       | 10.20             |
| worst                   | 11.80                      | **11.00**                      | 15.50             |
| over-8.3 / over-12.5 ms | 461 / 0                    | 433 / 0                        | 439 / **4**       |
| gen jobs                | 138 (11/s) avg 2.29 ms     | **1245 (100/s)** avg 1.29 ms   | 1245 avg 1.17 ms  |
| mesh jobs               | 5630 (451/s) avg 0.14 ms   | **43644 (3497/s)** avg 0.10 ms | 33367 avg 0.08 ms |
| busy (overlay)          | 1.02–1.04 ms               | 1.24–1.40 ms                   | 1.07–2.01 ms      |
| uploads / 5 s           | ~25 MB                     | ~200 MB                        | ~350 MB           |

**4 workers was measured and declined.** It fills 2.6× faster than 2 and still beats the inline path on p99 (10.20 vs
11.10), but it is the only arm that drops a frame at all (4 over 12.5 ms, worst 15.50) — and this change exists to buy
frame time, not to spend it. Shipping 2. Note the shipped cap is not what protects weak hardware: `default_count`'s
`cores - 2` is, so a 4-core machine gets 2 regardless and a 1–2 core machine gets none. Raising `MAX_WORKERS` only ever
affects machines with cores to spare.

Also note 4 workers does FEWER mesh jobs than 2 (33k vs 44k): a faster fill lands stack siblings together, so fewer
cross-border remesh cascades fire — the same effect the second pass's column-gen rework measured natively.

**Every frame percentile improved while the client did ~8× the streaming work.** That is the whole thesis: the streaming
tax left the frame. `busy` rising 1.0 → 1.4 ms is the absorption that CANNOT follow the jobs off-thread (store
bookkeeping, `write_buffer` — both pinned to the thread owning the device), and it is a fifth of what the jobs were
costing.

**Per-job cost is ordinary, and an earlier scare was a misreading.** A run taken while the pool was feed-starved
reported gen avg 5.85 ms and looked like a 5× wasm penalty. It was the menu BACKDROP world's terrain — cave noise makes
column cost bimodal, and the earlier 1.17 ms web figure was taken there too, not at the seed-42 spawn. At the same pose
it is 1.29 ms in a worker against 0.83–1.01 ms native, i.e. wasm costs ~1.4×. Workers also beat the main thread per job
(1.29 vs 2.29 ms) because an inline job's wall clock includes competing with the render loop.

**Read the `workers=0` arm as floor-only, not as the shipped client.** `?bench` disables the adaptive allowance
(`adaptive_effective`), so that arm runs the fixed floor — and the tell is in the table: 451 mesh/s is `INLINE_MESH_CAP`
(4) × 120 fps almost exactly. A user on today's adaptive inline path sits somewhere better than this. The
shipped-vs-shipped number needs overlay readings on a NON-bench run, which is still owed.

Two bugs found by taking this measurement at all, both worth remembering:

- **The pool was feed-rate starved at first**, capping throughput at `DEPTH × workers × fps` = 480 jobs/s with the pool
  9% utilized and 1124 columns queued. `top_up_workers` refills once per `maintain`, so a worker that empties its queue
  idles until the next frame. This is the SAME starvation the second pass found natively with `bounded(workers*2)`, and
  the same fix: depth 2 → 16.
- **`?bench` opened the menu**, because `EngineConfig::web()` starts with `menu: true` while desktop starts menu-less.
  The flight still ran (it returns before the interactive half), so this cost comparability and appearance rather than
  correctness — but "capture paths must not sit on a menu screen" is a documented rule and the web bench was breaking
  it.

## 2026-08-14 — what the web's inline executor actually costs

Groundwork for the browser's Web Worker chunk pool. `--workers 0` IS the web's executor (`Pipeline::build` forces it on
wasm), so a native bracket measures what that platform gives up — modulo one caveat that turns out to matter.

M2 Max, 120 Hz vsync, `--bench --display 0 --fly 1 --frames 4800`, seed 42, draw 20. `--fly 1` freezes the camera after
one frame, so this is the initial fill with no travel demand. Over-12.5 ms counts are out of 4739 scored frames.

| workers    | fill        | avg  | p95  | p99        | over-12.5 ms   |
| ---------- | ----------- | ---- | ---- | ---------- | -------------- |
| 0 (inline) | **113.4 s** | 8.47 | 9.64 | 16.52      | 687 (of 21939) |
| 1          | 23.4 s      | 8.34 | 9.00 | 10.04      | 9              |
| 2          | 9.8–10.1 s  | 8.34 | 8.73 | 9.21       | 5              |
| 4          | 3.6–3.7 s   | 8.35 | 8.90 | 9.38–11.41 | 19–41          |

The inline row needed a 22000-frame run to finish at all; at 4800 frames it never completed the fill, having managed 425
of the 1245 gen jobs in 40 s. On the travelling flight (`--bench-speed 30`, 15 s) it is 6× fewer gen jobs and 13× fewer
mesh jobs than 4 workers, and never fills.

**Read the inline row as a lower bound, not as the shipped web client.** `--bench` disables the adaptive allowance for
determinism (`adaptive_effective`), so `--workers 0` there runs the floor batch ONLY — one `Generate`, or
`INLINE_MESH_CAP` meshes, per frame. The shipped web build gets an allowance equal to its whole budget, observed at 5.6
ms while streaming (see the 2026-08-06 web readings) ≈ 0.67 core-seconds/s, which against ~5.4 s of wasm job work puts
its real fill in the 8–10 s range — i.e. roughly where 2 workers land, not where 113 s does. **On a machine with
headroom the adaptive budget already buys most of what a worker pool would.** The case for the pool is the hardware
WITHOUT headroom, where the controller collapses to its 3.0 ms floor while job costs rise 3–4×, plus the floor batch
that no controller can shed at all.

**A retraction.** A first pass of this bracket, taken on a thermally loaded machine right after a release build, read
340 over-12.5 ms frames at 4 workers against 30 at 2, and was briefly used to argue a hard pacing knee at 2. Re-measured
cold, twice, it is 19–41 against 5 — the direction survives (absorbing results is pinned to the thread that owns the
device, so a faster pool spends more main thread per frame, not less) but the cliff does not. Natively the 4-worker
trade is still worth it. Two remains the WEB default on grounds this bracket cannot measure: per-worker wasm linear
memory on low-RAM devices, and a `upload_bytes_max` of 768 KiB a frame against native's 8 MiB, which must move the knee
earlier there. Re-measure on web before raising it. Lesson for the next bracket: pin the thermal state, not just
`--display`.

## 2026-08-14 — what a torch in your hand costs (R28)

M2 Max, `--bench --warp 0` (parked in the sealed vault, no motion), `--fixed-dt 0.0166667`, seed 42, tod 45,
`--display
0` pinned, `DIG_VAULT=1 DIG_VAULT_DARK=1 DIG_SCREEN=torch` — a room whose ONLY light is the one in the
player's hand. Interleaved, 2 reps per arm; within-arm spread **0.08 / 0.02 ms**.

| `--gi-scale`                   | resolution       | baseline      | `DIG_STUB=ginee` | term           |
| ------------------------------ | ---------------- | ------------- | ---------------- | -------------- |
| 1 (resolve at full render res) | 5.79 MP, draw 20 | 19.76 / 19.68 | 17.75 / 17.73    | **1.98 ms**    |
| 3 (**the shipped default**)    | 5.79 MP, draw 20 | 8.33          | —                | not measurable |

**1.98 ms at `--gi-scale 1`, and it is the worst case by construction.** A light at the eye puts every visible pixel
inside `LIGHT_RANGE` with `ndl > 0`, so every one of them draws a candidate and fires a shadow ray — R20's sealed-vault
worst case, except now it is the *normal* case for anyone carrying a torch. And the vault is eleven blocks across, so
there is no far band to early-out. At the shipped divisor the resolve runs at a ninth of the pixels and the same scene
sits at the 120 Hz cap with headroom to spare, which is why that row has no number: nothing to subtract from a frame
that is not GPU-bound.

**No existing baseline moves, and the reason is worth writing down.** `--bench` flies with an empty hotbar, so
`Swap::shown()` is `None`, no carried light exists, and `flush_lights` behaves exactly as it did. Verified rather than
assumed: the CSV's `emitters` column reads **0** on every pre-existing bench scene and **1** here.

**The harness could not see a held item at all, and that had to be fixed first.** `update` returned on the
scripted-flight path *before* advancing `Swap`, so under `--bench` the viewmodel never drew and a carried light never
existed — the first attempt at this bracket measured `ginee` at 0.00 with `emitters` 0 and looked like a correct result.
Hoisting the one `view_swap.advance` above that return costs the old baselines nothing (empty bar → no item either way)
and is what makes a held item photographable and measurable.

**The picture, same scene, 960×540, 600 frames.** Mean interior **58.6/255** with the torch in hand against **26.8**
holding a non-emitting item — the carried torch is the room's light, not a tint on it. Clipping, R22's metric: **0.123
%** of pixels with any channel at 255, of which 2.1 % are actually white, against R22's **0.40 %** placed-torch
baseline. Carrying a torch clips *less* than standing next to one, and the near-field disc it throws at feet + 1.0 reads
as a warm pool rather than the saturated ellipse the arithmetic predicted. `cam_light` does not blow out either, for a
structural reason: a probe stores what its RAYS find, so a torch reaches the hand as bounce off the floor, never as
`emit·GAIN/LIGHT_NEAR²`.

**The CSV's `gpu_*` columns are unusable at this pose and that is not new.** They reported `gpu_gi_denoise_ms` of 34 ms
inside a 19.8 ms frame, and moved by the same ~2 ms in phases the stub cannot touch (`gpu_main_ms`, `gpu_viewmodel_ms`)
— including `refresh_ms`, which a shader stub cannot change at all. Take `avg_ms` from an interleaved bracket; the phase
timers here are a global clock artefact, exactly the trap the 2026-08-12 entry named.

## 2026-08-12 — the toroidal wrap was 25% of the frame

Same machine and pose as the attribution entry below: M2 Max, 5.43 MP (2992×1814),
`--bench --no-vsync --frames 1200
--fixed-dt 0.0166667`, seed 42, tod 45, draw 20. Interleaved, 3 reps per arm with the
first discarded — spread **0.02 ms**.

|           | avg ms   | fps       | p95       | p99       | over-12.5ms |
| --------- | -------- | --------- | --------- | --------- | ----------- |
| baseline  | 11.75    | 85.1      | 16.33     | 18.53     | 458         |
| **after** | **8.83** | **113.3** | **12.35** | **15.09** | **51**      |

**−2.92 ms, −24.8%, and 89% of the over-budget frames gone**, for four lines of WGSL that change no arithmetic result.
Capture is pixel-identical: mean **0.00002**/255, max 1, 0.0024% of pixels, against a 0.11% capture-noise floor.

**It was address math, not shading.** `gi_cell` and `gi_coarse_empty` wrapped the toroidal window with the textbook
positive-modulo `((c % s) + s) % s`. On a GPU with no integer divide unit each `%` is an emulated sequence, and the
marches ran the pair twice per iteration — once for the coarse brick, once for the fine cell — over two wrapping axes.
**Eight emulated divisions stood between every DDA step and its texture load**, in the loop the stub table below
attributes 40% of the frame to. Two changes, each measured on its own:

1. **One modulo per axis** (`gi_wrap2`) — the second only fixes a negative first result, which a compare and an add do
   for free. 11.75 → ~9.6.
2. **One wrap for both levels of the skip** — the window side is a multiple of 8, so `wrap(c, side) >> 3` IS
   `wrap(c >>
   3, side / 8)`, and the brick address is three shifts off the fine one. ~9.6 → 8.83.

Attribution first, which is what pointed here (baseline 14.84 on a noisier machine, so read the shares, not the
absolutes): `giflat` 6.48 ms / 44% of the frame, `gigather` 6.22 / 42%, `gisun` 3.37 / 23%, `giatrous` 1.21, `noscene`
0.93, `nowater` 0.56. The final gather is essentially the whole resolve, and the gather is the DDA.

### The walking soak — the win widens, the ratchet does not go away

`--bench-speed 4 --frames 13500` (225 world-seconds, ~900 blocks), compared at the same POINT ON THE WALK (cumulative
frames), not the same wall-clock bucket — the faster build is further along at any given second and the naive comparison
flatters it.

| ~frame      | 1000  | 3000  | 5000  | 7000  | 9000  | 11000 | 13000 |
| ----------- | ----- | ----- | ----- | ----- | ----- | ----- | ----- |
| baseline ms | 10.57 | 10.96 | 17.15 | 15.96 | 14.99 | 15.08 | 15.15 |
| after ms    | 6.10  | 7.28  | 10.26 | 10.49 | 8.69  | 8.87  | 9.53  |
| gain        | −42%  | −34%  | −40%  | −34%  | −42%  | −41%  | −37%  |

Whole-run: **14.58 → 8.84 ms**, hitches 216 → 1. The gain is *larger* at walking pace than on the flyover, because the
slow path is more DDA-bound and less streaming-bound.

**The session ratchet is scaled, not removed.** Baseline climbs 10.6 → 15.1 (+43%) across the walk; the fixed build
climbs 6.1 → 9.5 (+56%). Both degrade by about the same FACTOR, which is the expected shape if the ratchet is "more DDA
iterations per ray" and this change is a constant factor on the cost of one iteration. It confirms the 2026-08-12 drill
finding from the other direction: the decline is real geometry, and the lever on it is per-iteration cost.

**Nothing accumulates on the CPU.** Across the whole 225-second walk, `busy_ms` moves 2.66 → 3.33 (tracking chunk count,
identical on both arms), `gpu_bytes` 208 → 261 MB (mesh density, identical on both), `emitters` 0 throughout, and
`sky_y` 183 → 239 exactly as the drill predicted. No unbounded container, no leak.

### Measured and rejected — do not retry

- **Cache the last brick index** and skip the coarse load while the march stays inside it. The redundancy is real (a
  dense brick is re-asked up to 8 times), but the load is a hit in a 51 KiB texture and the extra live register plus the
  divergent branch cost more: **12.02 → 14.03 ms.** Pinned in a comment at `gi_coarse_empty_w`.
- **Replace `gi_ray_skip_brick`'s three float divides** with multiplies by `max(abs(dir), 1e-6)` (which is `1/inv` by
  construction). No measurable change, and it puts an ulp of slop into a function whose comment records that the
  position form was measurably wrong. Reverted.
- **The same wrap fix in `gi_probe.wgsl`.** Neutral within noise — the probe compute is ~0.9 ms of the frame, not 6.
  Kept anyway, because the two steppers are pinned twins and must not drift.

### Caveat on the machine

Three soak pairs were thrown away to background macOS daemons (`duetexpertd`, `speechmaintenanced` pegging two cores;
load average 18). The tell is `gen jobs avg` rising 0.85 → 2.6 → 4.1 ms and a `worst` in the hundreds of ms — the GPU
numbers move with it. Check `uptime` before trusting a bench, and note that the clean baseline here (11.75) reproduces
the 11.79 recorded in the attribution entry below, which is what confirms the machine was in the documented state.

## 2026-08-12 — render scale, GI scale, and which one to actually spend

Same machine and pose as the attribution entry below: 5.43 MP (2992×1814),
`--bench --no-vsync --frames 1200 --fixed-dt
0.0166667`, seed 42, tod 45, draw 20. Interleaved, 2 reps — spread ≤0.09
ms.

| render scale | GI divisor | avg ms   | vs native | image Δ vs native                         |
| ------------ | ---------- | -------- | --------- | ----------------------------------------- |
| 1.0          | 2          | 12.50    | —         | —                                         |
| 0.85         | 2          | 10.22    | −18%      | —                                         |
| 0.7          | 2          | 8.74     | **−30%**  | mean **18.61**/255                        |
| **1.0**      | **3**      | **8.72** | **−30%**  | mean **0.11**/255, max 76, 4.7% of pixels |
| 0.7          | 3          | 8.33     | −33%      | —                                         |

**The two buy the same 30%, and the GI notch costs ~170× less picture.** That is not intuition — the plan had render
scale as the headline lever and GI-ratio decoupling parked in reserve as "a genuine quality trade". It is the other way
round, because the à-trous filter and the bilateral upsample already reconstruct that signal, while a render scale drop
resamples everything including the geometry edges this engine is made of.

So the interactive **high-DPI default is native resolution with lighting detail stepped down**
(`settings::density_defaults`), not a reduced render scale. Render scale remains a slider for anyone who needs more than
one notch's worth.

Applied once at the first `set_scale_factor` — the first moment anything knows the backing scale — and only when the
player has expressed no preference (no flag, nothing saved). Every capture path leaves `gfx_defaults_auto` false, so
bench/screenshot/drill stay on the shipped values and no recorded baseline moves. Verified on a throwaway profile: logs
`display scale 2x: render scale 100%, lighting detail /3` and writes no settings file, so the derived default follows
the display instead of freezing into a saved preference.

Parity: at render 1.0 / GI 2 the new plumbing is mean **0.0013**/255, max 1 against the pre-scale build — the `gi_div`
uniform reproduces `chunk.wgsl`'s old hard-coded `/ 2` exactly.

**Cost of the frame restructure itself** (water and the viewmodel moved onto a render-resolution `post` target, one
upscale pass to the surface), interleaved ×2 at render scale 1.0: **12.03 → 12.26 ms, +0.235 ms (+2.0%)** — the extra
native-res fullscreen pass. It inverts the moment either scale drops below native.

**Trap worth keeping:** `blit_pipeline` is built with `pipeline_builder::build`, which attaches a depth-stencil state,
and a pipeline carrying one may only run in a pass that HAS a depth attachment. It worked for years inside the water
pass, which has one. Reusing it in a standalone fullscreen pass invalidates the encoder — and an invalid pass poisons
the whole `CommandEncoder`, so `finish()` yields a command buffer where every pass is dropped. The symptom is a black
window with no HUD and all GPU timestamps reading 0.00, which looks nothing like a bad blit. `upscale_pipeline` is the
depthless twin. `RUST_LOG=warn` surfaces it as "Encoder is invalid"; filtering logs to `wgpu` targets hides it, because
the handler logs under `digblock_platform::app`.

## 2026-08-12 — GI attribution (`DIG_STUB` extended), and the window-size trap

M2 Max, **5.43 MP (2992×1814)** — `--size 1728x1117` clamps to the usable screen, so this is effectively fullscreen and
the same framebuffer as `shots/f8-64.png`. `--bench --no-vsync --frames 1500 --fixed-dt 0.0166667`, seed 42, tod 45,
draw 20. Rep 1 of an interleaved cycle (see the caveat at the end — this is one rep, not a settled floor).

`DIG_STUB` covered water only; the GI resolve — the frame's most expensive pass — had never had a single term
attributed. Added `gisun gigather ginee giflat giatrous` (resolve, riding its own `jitter.w` since it binds no frame
UBO) and `noscene noraster` (raster).

| `DIG_STUB`                        | avg ms | term cost | share of frame                                 |
| --------------------------------- | ------ | --------- | ---------------------------------------------- |
| *(none)*                          | 11.79  | —         | —                                              |
| `giflat` (whole resolve)          | 4.70   | **7.09**  | **60%**                                        |
| `gigather` (96-iter final gather) | 5.92   | **5.87**  | 50%                                            |
| `gisun` (320-iter sun ray)        | 9.02   | 2.77      | 23%                                            |
| `ginee`                           | 11.79  | 0.00      | none — no emitters in a vanilla world, correct |
| `noscene` (scene chunk raster)    | 11.85  | **~0**    | —                                              |
| `noraster` (all chunk geometry)   | 7.13   | 4.66      | confounded, upper bound only                   |

**The GI resolve is 60% of the frame, and the final gather is a bigger term than the sun ray** — the opposite of what
the iteration counts (96 vs 320) suggest, because the sun ray's sky-ceiling early-out fires on most upward rays while
the gather marches 32 blocks of real geometry. `noscene ≈ baseline` says the scene-pass chunk raster is free on this
TBDR: draw calls and triangles are a CPU-encode cost here, not a GPU one.

**The window-size trap, which is the finding that outlives the table.** The identical bracket at the DEFAULT bench
window measures *backwards* — `gisun` 7.19 vs a 6.28 baseline, `giflat` 6.88, every stub apparently *adding* time. That
frame is not GPU-bound, so removing GPU work changes nothing and run-to-run variance is the whole signal. Any GI
measurement taken at the default window size is worthless; use the real one.

Two stubs are confounded by construction and are documented as such in `renderer::stub`:

- `noraster` empties the depth buffer, so every resolve pixel takes the sky early-out and the most expensive pass goes
  cheap too. Never subtract it from `noscene` to get the prepass — there is deliberately no prepass-only stub.
- `giatrous` measured 12.19, *slower* than baseline, because skipping the filter also starves the history feedback into
  `[1]` and pushes more pixels down the prime path every frame. It is not the filter's cost. (The filter measured 0.14
  ms when it landed.)

**`gpu_gi_denoise_ms` changed meaning.** Both à-trous iterations wrote query pair 12/13, so the second silently
overwrote the first and the column reported the stride-2 pass alone. Each iteration now has its own pair and the column
sums them: it roughly doubles (5.65 → 10.66 on the same scene). Pre-2026-08-12 CSVs read about half. `gpu_depth_blit_ms`
and `gpu_viewmodel_ms` are new — both passes ran untimed since they were written; the depth blit is ~0.26 ms.

### The session ratchet — walking-speed soak

`--bench-speed B` makes the flyover's speed a flag; its default is every prior baseline's value. 30 blocks/s is 7.5×
`WALK_SPEED` and a different *streaming* regime, not merely a faster one: at that pace the GI upload budget cannot keep
up. `--walk` is not a substitute — its one-block oscillation never brings new terrain into the window.

`--bench-speed 4 --frames 13500` (225 world-seconds, ~900 blocks) at 5.43 MP:

| t_s     | 5     | 30    | 60   | 70   | 135  | 155  |
| ------- | ----- | ----- | ---- | ---- | ---- | ---- |
| fps     | 129.5 | 115.0 | 79.5 | 64.2 | 82.1 | 91.4 |
| `sky_y` | 183   | 183   | 199  | 223  | 239  | 239  |

`sky_y` is monotone and never falls, which looked like a ratchet: the CPU coarse mirror it is scanned from is never
cleared on eviction, so in principle it climbs toward the tallest terrain seen anywhere in the *session* rather than
anywhere in the *window*, and every non-descending ray in the resolve breaks on it.

### That hypothesis is REFUTED — do not spend time on it again

`--bench --fly 9000 --frames 12000 --gi-drill-at 10500 --bench-speed 4`: camera frozen at frame 9000, the drill (clear
the mirror + emitter map, `gi_redirty_all`) fires at 10500, against an identical control with no drill. Same pose, same
thermal state, same frame indices — the only difference is the GI's derived age.

| frozen-pose tail  | fps  | avg ms | `sky_y` |
| ----------------- | ---- | ------ | ------- |
| A, drill at 10500 | 83.8 | 11.93  | 223     |
| B, control        | 83.9 | 11.93  | 223     |

Whole-run avg **11.57 vs 11.56**. `sky_y` was cleared from 223 and **rebuilt to 223**. Clearing the derived state and
rebuilding it from what is actually resident changes nothing, because the state was already correct — 223 is the genuine
ceiling of the resident window, not a stale high-water mark. `sky_y` rising 183 → 239 across the soak is real terrain
entering the window as you walk.

So the reported "120 fps at spawn, 50 after wandering, never recovers" is **not stale GI bookkeeping**. It is the true
steady-state cost of a fully streamed world at full resolution, and the resolve owns 60% of it. The `GiDraw::emitters`
map is still an unbounded leak worth fixing on its own merits (nothing prunes it on eviction, and `flush_lights` sums it
every frame before its early-out), but it is latent: only `Torch` emits and worldgen places none, so a vanilla world
measures `emitters 0` throughout.

**Bonus: this pair is the repeat floor.** Two full runs differing by 0.01 ms whole-run avg puts the harness noise well
under any term in the attribution table above, so those single-rep numbers stand.

## 2026-08-11 — water attribution (`DIG_STUB`), R21

M2 Max, 5.4 MP (`--size 1496x907` on a 2× display — verify PIXEL dims, `--size` is logical),
`--draw-dist 32 --seed 42
--tod 45 --no-vsync --frames 1500`, settled interval of the metrics log. **Per-pass GPU
counters are not used here and should not be:** on this TBDR the passes in `shots/f8-64.png` sum to 46.4 ms inside a
19.68 ms frame, because the tiling stage of one pass overlaps the fragment stage of the last. The only honest number is
whole-frame time with one term removed at a time.

Pose: submerged, looking up at the surface — `--pos -280 131 -232 --yaw 150 --pitch 30`. Chosen because it reproduces
the user's report ("just as slow below the surface"), which no fragment-count hypothesis survives. Same-build repeat
floor **0.02 ms**.

| `DIG_STUB` | avg ms | saved    | share of water |
| ---------- | ------ | -------- | -------------- |
| *(none)*   | 14.85  | —        | —              |
| `wsun`     | 10.93  | **3.91** | 55%            |
| `wssr`     | 13.16  | 1.68     | 23%            |
| `wvert`    | 13.93  | 0.91     | 13%            |
| `wripple`  | 14.77  | 0.07     | —              |
| `wrefract` | 14.79  | 0.05     | —              |
| `spray`    | 14.71  | 0.13     | —              |
| `wflat`    | 8.52   | 6.32     | 88%            |
| `nowater`  | 7.69   | **7.15** | 100%           |

Water was 48% of the frame; without it the pose runs 130 fps. Both dominant terms were ray marches whose answers were
multiplied by ~0 (GI-DESIGN.md R21). After gating them on their own contribution:

| pose                                  | water before | water after | frame before → after        |
| ------------------------------------- | ------------ | ----------- | --------------------------- |
| submerged, looking up                 | 7.15 ms      | **1.40**    | 14.85 → 9.09 (67 → 110 fps) |
| submerged, flat                       | 5.22         | **1.20**    | 16.34 → 12.35 (61 → 81)     |
| above water, terraced river + cascade | 2.45         | **1.60**    | 13.65 → 12.74 (73 → 79)     |

Residual at the submerged pose: `wvert` 0.83 ms (now the largest water term), rest of the fragment ~0.6, the two marches
~0.05 each.

**Harness note that outlives these numbers:** cross-build image parity needs `--fixed-dt`. Everything animated rides dt,
so two builds of different speed reach `--frames N` in different world states; at DIG_RIVER that put a *same-change*
diff at mean 6.9/255 against a 0.69 same-build floor, entirely in the ripple bands. With `--fixed-dt 0.0166667` the
same-build floor is 0.0001 (max 1) and the real parity reads ≤0.0023.

## 2026-08-10 — Stage-B voxel light

A/B on the seed-42 flight, same machine/session, baseline = pre-light main (27f9a2a):

| metric      | baseline    | light (final) | note                                           |
| ----------- | ----------- | ------------- | ---------------------------------------------- |
| fill        | 4.29 s      | 3.71 s        | run-to-run variance band; not claimed as a win |
| avg         | 8.43 ms     | 8.41 ms       | flat                                           |
| p95 / p99   | 9.38 / 9.75 | 10.17 / 12.15 | the honest cost — fill-time skylight settling  |
| over-12.5ms | 10          | 9             | recovered (see below)                          |
| hitches     | 0           | 0             |                                                |
| mesh jobs   | 76 444      | 85 923        | +12%: chunks re-mesh once with settled light   |

Two tuning findings, both now load-bearing:

- **`LIGHT_BUDGET` cut per `step_light` site.** A pop costs ~6 hash-map neighbor reads; at 16k×2 per maintain a
  streaming backlog spent multiple milliseconds inside `maintain` and p99 went to 14.0 ms with 54 frames over 12.5 ms.
  4k×2 bounds the worst maintain near a millisecond; settling is later, never wrong (dirty-gating remeshes whatever
  settles late).
- **Dirty drains only when the field is SETTLED** (`pending() == 0`), not per budget slice. Per-slice draining re-bumped
  the same chunks every maintain while a backlog settled — +21% mesh jobs on the bench. One drain per settle = one light
  remesh per chunk. Test-side corollary: `quiesce` means chunk-quiet, not light-quiet — pipeline light tests must
  `settle_light` before asserting, or they race the budget.

Remaining p99 gap (~+2.4 ms) is fill-window skylight arrival work (descend scans + the settle drain's remesh burst)
under the vsync headroom; steady flight shows `light q 0` every report window and zero hitches.

Acting on the measurement pass below. The drain now refuses to step past a **hole** in the input tick sequence for up to
`INPUT_HOLD_MAX` ticks, so an input reordered by jitter is still admitted when it lands instead of arriving under the
high-water mark and dying there. `EDIT_POS_TOLERANCE` and `dig_grace` are deliberately **not** made depth-aware — see
the rule on `INPUT_HOLD_MAX` in `protocol.rs`.

The bench grew a second link table for this, because the constants a hold spends are edit-path constants and the sweep
only ever walked.

### Before (`INPUT_HOLD_MAX = 0`) → after (`= 4`)

| link     | in-deliv          | late/s     | rtt mean      | repl/s      | corr mean     | corr max        | depth     | orig max          |
| -------- | ----------------- | ---------- | ------------- | ----------- | ------------- | --------------- | --------- | ----------------- |
| lan      | 100.0 → **100.0** | 0.0 → 0.0  | 26.0 → 26.0   | 0.00 → 0.00 | 0.000 → 0.000 | 0.00 → 0.00     | 1.5 → 1.5 | 0.000 → 0.000     |
| wifi     | 99.9 → **99.9**   | 0.0 → 0.0  | 48.1 → 48.1   | 2.70 → 2.70 | 0.069 → 0.069 | 0.13 → 0.13     | 1.5 → 1.5 | 0.067 → 0.067     |
| cellular | 98.9 → **99.8**   | 0.6 → 0.0  | 142.1 → 142.8 | 3.85 → 3.85 | 0.078 → 0.078 | 0.20 → 0.20     | 1.7 → 1.8 | 0.266 → 0.267     |
| bad-4g   | 91.8 → **98.0**   | 8.2 → 1.8  | 299.5 → 310.4 | 3.85 → 3.65 | 0.093 → 0.090 | 0.33 → 0.33     | 1.9 → 2.5 | 0.599 → **0.399** |
| awful    | 73.0 → **89.5**   | 26.3 → 8.9 | 430.2 → 474.3 | 5.40 → 4.65 | 0.273 → 0.220 | 0.73 → **1.07** | 1.7 → 4.6 | 1.021 → **0.867** |

**The lan and wifi rows are unchanged to the digit.** Not approximately — identically, every column. A link with no
reordering has no holes, so the branch never fires. That is the whole cost argument: almost every player pays exactly
nothing for this.

**A hold does not spend `EDIT_POS_TOLERANCE` — it saves it.** This was the prediction the sweep refuted, and it is the
most useful thing it found. The arithmetic said a deeper buffer must push the claimed edit origin further from the
server's simulated position, at 0.067 blocks per tick of walk. The measurement says `orig max` *falls*: 0.60 → 0.40 at
300 ms, 1.02 → 0.87 at 500 ms. The reason is that the alternative to holding is coasting on stale intent, and a coast
diverges faster than a delay does. The tolerance headroom was never the binding constraint at this cap.

**What it costs is latency, and only on links that already have some**: +0.7 ms at 120 ms, +11 ms at 300 ms, +44 ms at
500 ms. Client prediction hides all of it for movement; it is visible only where the server's current position is
compared against the client's, which is the `orig` column above — and that got better, not worse.

### Honest debits

`corr max` at 500 ms goes the wrong way: 0.73 → 1.07 blocks. It is one deterministic event (identical at caps 2, 4, 6
and 8), and it is the tail of the mechanism — when a hole never fills, the server coasts the full cap and then skips,
which is a single larger divergence than skipping immediately. The body of the distribution improves: `corr mean` −19%
and `repl/s` −14%, so total corrective motion falls from 1.47 to 1.02 blocks/s. Fewer, smaller corrections with one
rarer, bigger one — worth it, but not the unambiguous win the acceptance criterion assumed.

`dig ok/att` at 500 ms reads 7/8 → 6/8. **This column cannot rank the cap**: stone is 90 ticks, so a 20 s row gets 8
attempts and the sweep bounces around 4–8 with no trend. `dig short` is zero on every row at every cap, so `dig_grace`
is not what is failing — the abandonments are `DIG_OFF_TARGET_RESET`, the server's `look` lagging far enough at 500 ms
that a hold exceeds 30 consecutive off-target ticks. That is the known aim-has-no-adoption-envelope problem, not a
regression from this change, but a longer-window dig measurement would be needed to say whether a hold makes it
meaningfully worse.

### The sweep the cap was chosen from

`in-deliv` by cap:

| cap      | 0    | 2    | 4        | 6    | 8    | 12   |
| -------- | ---- | ---- | -------- | ---- | ---- | ---- |
| cellular | 98.9 | 99.8 | 99.8     | 99.8 | 99.8 | 99.8 |
| bad-4g   | 91.8 | 96.5 | **98.0** | 98.2 | 98.2 | 98.2 |
| awful    | 73.0 | 82.5 | **89.5** | 93.0 | 94.3 | 93.7 |

and what the higher caps cost, on the `awful` row:

| cap       | 0     | 2     | 4         | 6     | 8     | 12       |
| --------- | ----- | ----- | --------- | ----- | ----- | -------- |
| rtt mean  | 430   | 455   | 474       | 484   | 487   | 488      |
| repl/s    | 5.40  | 4.45  | **4.65**  | 5.10  | 5.95  | 6.25     |
| orig max  | 1.021 | 0.799 | **0.867** | 1.132 | 1.328 | 1.787    |
| tol-rej/s | 0     | 0     | 0         | 0     | 0     | **0.05** |

4 is the knee. bad-4g is done there — 6 and 8 add two tenths of a point between them. awful keeps improving to 8, but
buys it with a replay rate that climbs back *above* the no-hold baseline, which is the symptom the change exists to
remove. And 12 is where the arithmetic finally bites: `orig max` 1.79 exceeds the 1.5 tolerance and honest edits start
bouncing — the only non-zero `tol-rej/s` anywhere in the sweep. The bench catches the failure mode before a player
would.

Both halves stay deterministic: the link and edit tables are bit-identical run to run. The capacity table is wall-clock
and is not — `tick us` swings ~30% with machine load — but a held tick does strictly less work than a consumed one, so
the capacity conclusion below is untouched.

## 2026-08-09 — netcode measurement pass (`just bench-net`)

**This supersedes the baseline below it**, which was partly measuring the benchmark. Nothing in the engine changed: the
harness was fixed, the server input path was instrumented, and the numbers were re-taken. Recorded as a second entry
rather than an edit because the corrections are the finding.

### What was wrong with the first baseline

| column               | defect                                                                                                                     | effect                                                                                                                     |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `seen rtt`           | the wire delivered once per tick, checked against the same `now` the send used, so `at = now + one_way` was never `<= now` | every leg cost a whole 16.67 ms tick regardless of the configured link                                                     |
| `snap/s`, `replay/s` | read from `StatsAcc`, which publishes a closed 1-second bucket                                                             | a phase of the run, not the run — wifi printed 20.7/s against a 20.0 nominal                                               |
| `seen rtt`, `+-jit`  | EWMA seeded during warmup and never reset                                                                                  | the 4 s startup transient rode into the reported value                                                                     |
| `alloc/t`            | counters bracketed the whole loop body while the timer bracketed only `tick_once`                                          | the harness's own `Vec<SimEvent>` and per-client `vec![]` were being reported as the server's — ~74% of the number at n=64 |
| `tick us`            | claims were `[0, -500, 0]`, so the `sane` chain short-circuited on `POS_TOLERANCE`                                         | `probe_solid` → `overlaps_solid`, the most expensive call in the input path, was never run                                 |

The link half now sub-steps the wire and the client 8× per server tick (2.08 ms). The server still ticks at 60 Hz — a
packet arriving mid-tick genuinely waits for the next boundary, and that quantization is real. Every link column is
measured over the window rather than sampled from a rate bucket, and clients claim a position predicted with the
server's own physics and re-seeded from server state each tick, so adoption really happens (the harness asserts it
rather than assuming it).

### Server capacity — real worldgen, everyone walking, 600 ticks/row

```
 n |    tick us: p50    p95    p99    max | snap B | kB/s/c | alloc/t   alloc | snap
 1 |    0.8    1.2    1.8   25.1 |   31.0 |    0.6 |     0.6      88K |  100%
 8 |    4.7    7.9   20.2  132.8 |  194.1 |    3.8 |     3.3    1796K |  100%
24 |   12.9   44.5   65.9   84.5 |  567.0 |   11.1 |     9.3    7402K |  100%
40 |   18.2   72.2  114.9  120.6 |  939.0 |   18.4 |    14.9   13969K |  100%
64 |   29.1  138.0  199.8  208.5 |  937.3 |   18.4 |    23.1   19118K |  100%
```

p99 at 64 players is 200 us against a 16 667 us budget — 1.2%. The conclusion from the first baseline survives: interest
management, not tick cost, is what bounds player count.

`alloc/t` at 64 players is **23.1, not the 88.6 first reported** — three quarters of that figure was the harness
allocating its own input events. What remains is ~0.36 allocations per client per tick.

Adding the adoption path did **not** move `tick us` measurably. That is itself a result: `overlaps_solid` is cheap next
to residency and snapshot assembly, so the per-client cost is dominated by interest management rather than by validating
movement.

### Link quality — real NetClient vs real Sim over a seeded impairing wire

Flat world on purpose: this half measures the protocol, and real worldgen would put worker-thread completion times into
the loop. Rows are bit-identical run to run, so a difference is a difference in the netcode.

```
link     | cfg rtt  loss | drop | rtt mean  p95 | snap/s | repl/s | corr mean  max | max div | in-deliv | late/s want | depth
lan      |     4ms    0% |   0% |    26.0   35.4 |   20.0 |   0.00 |     0.000 0.00 |    0.07 |   100.0% |   0.0    0 |  1.5
wifi     |    30ms    1% |   1% |    48.1   70.8 |   19.8 |   2.70 |     0.069 0.13 |    0.33 |    99.9% |   0.0    0 |  1.5
cellular |   120ms    3% |   4% |   142.1  193.7 |   19.4 |   3.85 |     0.078 0.20 |    0.67 |    98.9% |   0.6    5 |  1.7
bad-4g   |   300ms    5% |   6% |   299.5  385.5 |   18.9 |   3.85 |     0.093 0.33 |    1.06 |    91.8% |   8.2   10 |  1.9
awful    |   500ms   10% |  12% |   430.2  554.2 |   17.8 |   5.40 |     0.273 0.73 |    1.81 |    73.0% |  26.3   18 |  1.7
```

**The `lan` row's RTT is 26.0 ms, not 41.9.** The old figure was ~16 ms of harness quantization. 26.0 lands next to the
24.8 ms measured live against a real QUIC server, which is the independent check that the model is now honest.
`seen
rtt` still exceeds the wire on purpose — it is the input→ack loop, so it carries the server's jitter buffer, the
60 Hz tick boundary and the snapshot cadence, which is the latency a player feels. It does **not** carry the send
stride: `sent_at` is stamped at transmission (`predict.rs:154`), so a tick waiting its turn on the wire is felt but
never counted. The first baseline claimed otherwise.

**`depth` is the finding.** The server's jitter buffer holds ~1.5 ticks on a clean LAN and never reaches 2 on any row —
including the calm ones. The drain in `step_physics` takes at least one input whenever the buffer is non-empty and has
no way to take zero, so depth is a one-way ratchet: an underrun destroys a tick of it permanently and nothing puts it
back. The first baseline said `JITTER_BUFFER_TICKS` was "too small to absorb ±150 ms". That was the wrong diagnosis —
the constant is not the mechanism, and raising it would change nothing. Being able to *hold* would.

**`late/s` and `want` corroborate `in-deliv` from the server's own side.** Late-lost inputs — arrived below the
high-water mark having never been held, so lost for good rather than being ordinary redundancy copies — are zero on lan
and wifi and climb to 26/s at 500 ms/10%. `want` says how deep the buffer would have had to be to still be holding the
worst of them: **10 ticks at bad-4g and 18 at awful**, i.e. 167–300 ms. Any fix aimed at those rows has to be willing to
buy that much depth, and every tick of it is latency paid by that player on every input — plus grace handed to anyone
who fakes jitter, since `dig_grace` and `EDIT_POS_TOLERANCE` are both sized against this lag.

Corrections stay small throughout (max 0.73 blocks at 500 ms/10%) and are smoothed into the camera over ~200 ms, so the
adoption envelope is holding well past any link this game will realistically see. That is the reason the delivery
shortfall is a polish item rather than an emergency: a dropped input is mostly overwritten by the next adopted claim.
What the envelope does *not* repair is `client.look`, which is written only from a consumed input and is what
`verify_digs` re-raycasts from every tick — so lost inputs degrade the server's aim, not just its position.

## 2026-08-09 — netcode baseline (SUPERSEDED, see above)

First multiplayer numbers in this file; the netcode had none. Apple Silicon (8P+4E), release build. Two halves, because
they answer different questions: server COST, and what a session FEELS like.

### Server capacity — real worldgen, everyone walking, 600 ticks/row

```
 n |    tick us: p50    p95    p99    max | snap B | kB/s/c | alloc/t   alloc | snap
 1 |    0.8    1.3    1.8   16.6 |   31.0 |    0.6 |     2.8     111K |  100%
 8 |    5.6    9.9   23.8   53.3 |  192.5 |    3.8 |    12.5    2444K |  100%
24 |   11.0   40.2   57.8   70.3 |  561.8 |   11.0 |    34.6   10469K |  100%
40 |   19.9   92.4  149.6  192.8 |  930.2 |   18.2 |    56.4   19385K |  100%
64 |   31.5  199.1  271.6  342.0 |  930.3 |   18.2 |    88.6   26669K |  100%
```

p99 at 64 players is 272 us against a 16 667 us tick budget — 1.6%. The sim thread is nowhere near its limit; interest
management, not tick cost, is what bounds player count. `snap B` flattening at 40 IS `MAX_SNAPSHOT_PLAYERS` working.
Per-client downstream tracks the v14 measurements (24 players: 562 B/snapshot and 11.0 kB/s here against 539 B and 10.8
kB/s measured standalone) — an independent confirmation of that change rather than a restatement of it.

### Link quality — real NetClient vs real Sim over a seeded impairing wire

Flat world on purpose: this half measures the protocol, and real worldgen would put worker-thread completion times into
the loop. Rows are bit-identical run to run, so a difference is a difference in the netcode.

```
link     | cfg rtt  loss | seen rtt  +-jit | snap/s | replay/s | corr mean  max | max div | in-deliv
lan      |     4ms    0% |    41.9     8.5 |   19.7 |     0.00 |     0.000 0.00 |    0.13 |   100.0%
wifi     |    30ms    1% |    56.7    11.1 |   20.7 |     3.93 |     0.067 0.07 |    0.33 |   100.0%
cellular |   120ms    3% |   154.6    20.7 |   18.7 |     5.90 |     0.075 0.20 |    0.67 |    99.4%
bad-4g   |   300ms    5% |   306.7    38.4 |   19.7 |     5.90 |     0.132 0.79 |    1.51 |    91.7%
awful    |   500ms   10% |   415.0    65.0 |   17.7 |     4.92 |     0.243 0.80 |    1.78 |    74.4%
```

Two things worth reading carefully.

**`seen rtt` exceeds the wire RTT by ~38 ms, and should.** It is the input→ack loop: send stride (0-33 ms) + the
server's jitter buffer (33 ms) + snapshot cadence (0-50 ms). That is the latency a player feels, which is the number
worth showing; a ping would report the 4 ms and explain nothing. Confirmed live against a real QUIC server on loopback
at 24.8 +- 8.5 ms.

**`in-deliv` falls off on the last two rows, and JITTER causes it, not loss.** Heavy jitter reorders datagrams;
`handle_inputs` skips anything at or below `last_input_tick`, a high-water mark, so a tick arriving after a newer one
was consumed is discarded rather than applied out of order. At `JITTER_BUFFER_TICKS` = 2 (33 ms) the buffer cannot
absorb +-150 ms of reordering. Deepening it would fix these rows and cost every other player latency — a tradeoff to
make deliberately and adaptively, on measured jitter, not by raising a constant. Left as a finding.

Corrections stay small throughout (max 0.80 blocks at 500 ms/10%), and they are smoothed into the camera over ~200 ms,
so the adoption envelope is holding up well past any link this game will realistically see.

All numbers: Apple Silicon (12-core: 8P+4E), release build, `just bench` (seed 42, tod 45, 1800 frames, scripted flight,
inputs off), ×3 runs.

## Earlier passes

Entries dated 2026-08-06 and before were deleted: they measure the pre-RTGI raster renderer on wgpu 26, so no figure in
them can be compared against a current run. `git log -- BENCH.md` has them.
