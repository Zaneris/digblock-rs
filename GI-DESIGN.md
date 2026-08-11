# GI-DESIGN — probe-traced global illumination

Status: **M4 landed on `feat/rtgi`.** RTGI is the only lighting path and there is no flag.
The raster stack it replaced is DELETED, not dormant: shadow.rs/shadow.wgsl and the
cascades, core/light.rs and the Stage-B BFS voxel light field, baked per-vertex AO, all fog,
the hand-tuned water look, and per-block tint jitter. LIGHT-DESIGN.md went with them.

What lights the world now, in one sentence per stage:

- A GPU **voxel window** (R8Uint, toroidal, `draw_dist` chunks on a side) plus an 8-block
  coarse occupancy texture, both fed by byte- and clock-budgeted CPU classification.
- Two **toroidal ambient-cube probe cascades** (C0 at 2-block spacing, C1 at 8), updated by
  a compute kernel over a permuted round-robin: one workgroup per probe, 32 rays, folded to
  6 axes, blended with a hysteresis ladder. Rays terminate in last frame's field, which is
  where multi-bounce comes from, and take one **next-event sample** against the emitter
  list per ray.
- A **half-res resolve** per frame: TWO per-pixel rays. One **final gather** — a
  cosine-weighted hemisphere ray about the exact normal, marched through the voxel window
  and terminated in the probe cache at whatever it hits, so a single ray buys the whole
  multi-bounce series. And one **sun ray** along the exact, unquantized caster jittered over
  a 0.5-degree cone. The result **accumulates temporally** through a reprojection of last
  frame's buffers, alpha = 1/n with n climbing to 512. Beyond the window's reach there is
  nothing to march, so those pixels keep the plain cascade gather.
- Solids bilaterally upsample that against exact face normals rasterized by the depth
  prepass. Water and players gather the probe field per fragment through gi_common.wgsl;
  spray and the viewmodel read a 32-byte camera-light buffer a 1-invocation compute writes.
- One **participating medium** (medium_common.wgsl) replaces every fog model, in-scattering
  the same `sky_base` the sky pass draws.

Capture envs: `DIG_VAULT` (torch room — the NEE test), `DIG_ROOF` (sun under an open roof),
`DIG_DIVE` (flooded tank), `DIG_SHAPES` (slab/stair tops must be lit).

## What is NOT built

Named here because this document previously described several of these as done:

- **No neighbourhood clamp.** The a-trous filter exists (`gi_atrous.wgsl`, two iterations),
  but the history is validated and travel-capped rather than clipped to the current frame's
  local range. A clamp needs the current frame's neighbours, which means splitting the
  resolve into a raw trace and an accumulate pass; at 1 spp its value is also doubtful,
  since a 3x3 of one-ray samples has a sigma wide enough to admit almost anything. SVGF
  itself does not use one. Disocclusion is still rejected rather than filtered.
- **No per-pixel NEE.** The gather ray reaches emitters only by hitting them; the `lights`
  buffer is read by the probe kernel alone. A torch is therefore lit by the cascades plus
  whatever the gather ray happens to strike, not by a shadow ray drawn against the list.
- **No tier scaling, and that is now a DECISION rather than an omission.** Every platform
  gets desktop budgets and an uncapped /2 resolve. The web row of the original design was
  never implemented, and web keeps desktop settings for the time being.
  What is known: at a matched 2992x1592 with the world quiesced, desktop runs 11.74 ms and
  the browser is the slower of the two — but by how much is NOT established, because Chrome
  caps requestAnimationFrame on an unfocused window (measured: `hasFocus:false`,
  `rafHz:60.1`) and every web reading drifted downward over ~60s as focus settled, with CPU
  `busy` flat at ~1.2 ms while `out` grew. Any ratio quoted from those runs is measuring
  throttling as much as cost. M5's cadence cut (-19% frame time) applies to both platforms
  equally, so whatever the gap is, it narrowed.
  Whoever picks it up: re-measure with the Chrome window genuinely focused for the whole
  run, and note that capping the half-res target is not a one-line change — chunk.wgsl's
  bilateral upsample hard-codes the ratio (`base = (vec2i(i.pos.xy) - 1) / 2`), so the real
  gi/full ratio has to be plumbed in first.
- **The atlases are still ping-ponged 3D textures** with a full GPU->GPU copy per frame.
  That copy was M5a's headline target and it was measured out: it costs **9 microseconds**,
  0.1% of the frame. Pricing it by DELETING it says the opposite (the atlas then holds
  garbage, which changes the data and the downstream workload with it — the frame got 6%
  *slower*); pricing it by ADDING four more is exact, because the result stays bit-identical.
  The storage-buffer rewrite would touch five files and put storage buffers in fragment-stage
  bind groups on WebGPU, which is real portability risk for a tenth of a percent. The
  cleanliness argument — nothing ever sampled these with hardware filtering, so the texture
  buys only the copy it forces — still stands on its own.
  `gather_prev` DOES now bound itself to the cascade's coverage and validate the toroidal
  checksum, matching the fragment side (M5b).
- **The voxel window covers half the streamed radius.** It is built `draw_dist` chunks on a
  SIDE (+/-160 blocks) while terrain streams to `draw_dist` chunks of RADIUS (+/-320). 9.2% of
  pixels therefore have a sun ray that leaves the window and is answered "fully lit" on no
  data, along a boundary centred on the camera, and the final gather is bounded to 127
  blocks by the same number. Doubling it costs 26 -> 105 MiB and measurably helps; see the
  R8 note for why it was not simply shipped.
- **No probe classification**, and **the axis fold is still lopsided** (`update_probes`
  barriers the workgroup and then leaves 6 threads alive, each running a serial
  RAYS-iteration reduction while the rest idle). Both were M5b items and both were measured
  out, because M5a removed their justification: after the cadence cut the probe pass costs
  **~0.33 ms of a 9.56 ms frame**, 3.5%, measured with an interleaved 512-vs-1024
  probes/dispatch bracket. Classification might halve that in a static scene and
  tree-reducing the fold takes a slice of it — 1-2% of frame each, for real complexity and,
  for classification, real staleness risk. Revisit only if the probe pass grows again.
- **No compute port.** The resolve is still a fragment pass. Its stated motivation was
  avoiding derivatives, which the rasterized normals removed, and tile classification,
  which nothing uses yet.

## Measuring motion

Every other capture in this engine is a still, which is the one case where a temporal
artifact has already converged away — so for a while the system had no way to photograph
its own worst failure mode. `--fly N` flies the bench path N frames and then FREEZES the
camera; `--warp N` lands on that same pose with no motion behind it. Pair either with
`--frames` to choose how long the camera then sits still. The pose is identical by
construction: `--warp` runs the identical step loop, because the yaw accumulates through N
`look` calls and a closed-form `yaw * N` differs in the last ulp — a subpixel shift across
the whole frame, which would swamp the diff being read. The flight is a pure function of
the frame index, so a repeat run reproduces at mean 0.007 / 0.00% of pixels — about 15x
tighter than the interactive capture-noise floor, which is what makes small differences
readable at all.

The A/B that matters is `--fly N --frames M` against `--warp N --frames M`: same pose, same
duration, same everything except whether the first N frames moved. On a correct temporal
system that difference decays to the noise floor.

**`--walk` is the second regime, and it is the one players report from.** The bench path is
a 30 blocks/s flyover twelve blocks up; a screen-space temporal artifact scales with how
fast the IMAGE moves, so that path photographs one end of the range and a walk photographs
the other — eye height, walking pace, ground a few blocks ahead, where one half-res texel
covers far more world per pixel. `--walk` modifies either `--fly` or `--warp` to swap the
flyover for an oscillation about the spawn point, one block forward and one back on a 1.5 s
period at pitch -25°. It oscillates rather than travelling so the column under the feet
stays fixed and the eye cannot step a whole block mid-path, invalidating history for a
reason that is not the one under test. Repeat noise is 0.007 (warp) and 0.003 (fly).

This matters more than a second data point: the smear R9 fixed measured 0.92 on the walk and
2.84 on the flyover, and it was the WALK crop that showed it as a picture — the flyover's
larger number is mostly disocclusion, which is not a bug. Read the two together, and crop
before believing a global mean: R9's artifact sat on one shadow and the flat foreground
diluted it by a factor of five.

**The first thing it caught: the accumulator froze.** The resolve blends with `alpha = 1/n`
up to `N_MAX = 512`, and the history was rgba16**float** — a 10-bit mantissa, so near a
stored value of 1.0 the ulp is ~1e-3 and `hi + (indirect - hi)/512` rounded to ZERO unless
the difference exceeded roughly 0.25. About a quarter of the signal range never converged:
each pixel froze at whatever partial value it held when the camera stopped, leaving a stale
pattern locked to the path walked, permanently. That was "weirdness that moves with you".

Measured before the fix: walked-vs-warped at the same pose stayed at mean 0.988 over 10.4%
of pixels after **25 seconds** of stillness, against 0.134 with accumulation switched off
entirely. Everything upstream was exonerated by measurement, not by argument — the voxel
window is bit-identical after a walk (max 0), the C0 probe field converges to a unique fixed
point (max 1), and disabling the sun cone jitter changed nothing. The N_MAX sweep grew and
then *saturated* (1 -> 0.134, 32 -> 0.337, 64 -> 0.761, 128 -> 0.921, 512 -> 0.979), which
is a quantization freeze and not slow convergence, and the residual was worst in the
brightest part of the frame exactly as float-ulp scaling predicts.

The fix is `gi::GI_ACC_FORMAT` = rgba32float: walked-vs-warped 0.988 -> **0.200** (the same
floor as no accumulation at all), frame-to-frame stability at rest 0.219 -> **0.032**, and
no measurable frame cost (bench 106.9 -> 107.0 fps) — the resolve is DDA-bound, not
bandwidth-bound. It costs ~15 MiB more VRAM for the four half-res targets, which is the web
tier's problem, not the desktop's.

This was the same failure the rgba8 target before it had ("in 8 bits that rounds to zero and
the average stops moving"), recurring one format up. The rule that generalizes is not "use a
float format" but that the mantissa must carry the smallest increment the history length
asks for; `history_format_resolves_the_accumulator` in gi.rs pins N_MAX against the format
so raising one without the other fails the build rather than the picture.

**What is left is the probe cache's own transient, and it is not a restart bug.** With the
freeze fixed, walked-vs-warped decays properly — 2.53 at 2 frames of settle, 1.69 at 30,
1.04 at 120, 0.67 at 300, 0.200 at 1500. Two plausible causes were tested and both were
measured out. Seeding a remapped probe from its converged neighbour toward the camera,
instead of restarting it at alpha 1.0 on one 64-ray estimate, changed **nothing** at any
settle length (2.530 -> 2.510, 1.693 -> 1.713, ...); injecting a deliberately absurd value
down that same path moved only 3.3% of pixels, which is the real lesson — an age-0 probe is
trilinearly averaged over 8 probes, bilaterally upsampled and then temporally accumulated,
so it barely reaches the screen. The premise was wrong too: a texel is remapped only about
every 128 frames of travel and is swept ~10 times in between, so the field is not
"permanently mid-restart" while you walk. And raising `PROBES_PER_DISPATCH` to give a
1-frame sweep — 16x the budget, no probe ever stale — bought only 21% at settle 2.

So the transient is inherent to a world-space cache at this cadence, and the answer was not
to sweep it harder but to stop making the near field depend on it. **That is what the final
gather did**, and the harness scored it: walked-vs-warped fell at every settle length —
2.510 -> 2.272 at 2 frames, 1.713 -> 1.176 at 30, 0.675 -> 0.364 at 300, 0.200 -> 0.133 at
1500 — while the cost went DOWN (107.0 -> 112.0 fps), because the 8-probe blend with a
relocation walk and an occlusion march at every primary pixel went away with it. One
cosine ray plus a 4-load cache lookup is cheaper than the gather it replaced.

Being a 1-spp stochastic estimate, it costs a little at-rest noise: frame-to-frame 0.032 ->
0.043, which is under a fifth of an 8-bit LSB.

**Then a player reported a smear, and it was the a-trous pass being missing.** Walking
forward dragged the shadow behind a block forward; walking back the lit ground ate it away.
`--walk` reproduced it: 0.922 mean, and cropped to the pit a feathered grey trail pulled
several blocks out of the shadow. The bisect was decisive — with accumulation switched off
the walked and warped frames agree to **0.001 over 0.00% of pixels**, so the voxel window
and both probe cascades are bit-identical either way and every bit of it lived in the screen
accumulator. At `N_MAX = 16` the smear is simply gone and the crop matches the still frame;
at 512 it is severe.

The mechanism is resampling, not reprojection: reprojection is exact for static geometry,
but the fetch still lands between texels, and carrying a running average through hundreds of
those drags the image along the direction of travel. Three things were measured, in order:

- **A nearest history tap was quantizing the fetch** (0.922 -> 0.695 walk, 2.84 -> 2.49
  flyover). Bilinear, validated per tap, replaced it. Note it changes the resting image by
  ~0.49 as well — concentrated at depth discontinuities, where a half-res reconstruction is
  ambiguous whichever tap it picks.
- **An adaptive cap keyed on the history's own 2x2 gradient did exactly nothing** — 0.695 ->
  0.695, max 56 -> 56. It read the gradient from the ALREADY-SMEARED history, so the smear
  flattens the edge that should have triggered it. Removed rather than tuned; a real
  neighbourhood clamp needs the current frame's neighbours, which a fragment pass has not got.
- **The cap belongs on reprojection travel**, which is the honest statement of when
  reprojection is lossless. Travel is measurably zero at rest — a deliberately hypersensitive
  `TRAVEL_K = 1000` still changed only 0.95% of pixels on a still camera — so a standing
  player keeps all 512 frames and nothing is special-cased about standing still.

None of that is affordable without a spatial filter, because a short history is a noisy one:
`gi_atrous.wgsl` is SVGF's other half, two edge-stopping iterations at strides 1 and 2. It
needs **no moments buffer**, which SVGF spends a third ping-ponged target on, because both
estimators here have an analytic variance from data already stored — sun visibility is a
mean of n Bernoulli trials, so its standard error is exactly `sqrt(p(1-p)/n)`.

Final: walk 0.922 -> **0.578**, flyover 2.84 -> **2.08**, and the crop is clean. Cost, on a
thermally settled machine, 110.4 -> **108.7 fps** (0.14 ms). The 5x5 B3 kernel SVGF specifies
was built and measured too — 0.522 for 0.31 ms — and is the one to reach for if the history
floor ever drops below `N_MOVING`. The filter changes a converged frame by 0.134 and skips
entirely above `N_CONVERGED`, where it measures as a no-op.

**The standing question this leaves is where the accumulator should live at all.** Screen
space is used everywhere in the literature but SHORT: SVGF-class denoisers accumulate 4-32
frames and take the rest spatially. Long-lived accumulation moved to world and surface space
precisely because it does not reproject — Lumen's Surface Cache, NVIDIA's SHaRC, EA SEED's
GIBS, and HPG 2024's on-surface caches, which drop the screen/world hybrid outright on the
grounds that "no reprojection is necessary in between frames, as the data is directly stored
on the surface". A voxel world gets that nearly free, since a face IS `(cell, axis, sign)`
where Lumen needs mesh cards and SHaRC needs a spatial hash. What stops it being obvious is
resolution: an affordable per-face cache is 4x4 texels per block face, and the sun's penumbra
three blocks from its occluder is ~0.05 blocks wide, so it would quantize a nearly-sharp edge
into quarter-block stairs exactly where a player is looking.

## Goals

- One mechanism lights everything: sun, sky, and torches are all radiance sources seen by
  the same rays. No shadow maps, no per-light machinery beyond a light list.
- Amortized over time, never in one frame: probes converge over fractions of a second with
  hysteresis; world edits and sun movement propagate as re-convergence, not recompute.
- GPU-resident: the web client is single-threaded — the CPU's only jobs are classifying
  edited chunks (µs) and byte-budgeted uploads.
- Low resolution with blending: coarse probes + a half-res resolve + temporal smoothing is
  the look; crispness comes from the two per-pixel rays — the sun ray, and the final
  gather. Baked AO is gone; contact darkening is what the gather ray does when the thing it
  hits is close.
- Fits WebGPU baseline `Limits::default()` — no feature/limit negotiation.

## Non-goals (v1)

- Specular GI / reflections (SSR stays as-is for water).
- Shadows beyond the voxel window (analytic fallback, mostly under fog).
- Sub-block emitter geometry: a torch lights from its cell, as a voxel.
- Colored transmission through water beyond a scalar transmittance.

## Literature

Majercik et al., *Dynamic Diffuse Global Illumination with Ray-Traced Irradiance Fields*
(JCGT 2019); *Ray Tracing Gems II* probe chapters (RTXGI scaling, relocation and
classification); NVIDIA RTXGI-DDGI; DDGI-Minecraft (Vulkan); *Real-Time Rendering* 4th
ed. ch. 11 (ambient-cube basis). Two deliberate deviations from stock DDGI, both possible
because we own an exact voxel occupancy texture: (1) no octahedral visibility payload —
leak suppression is exact short DDA occlusion at resolve time plus probe relocation;
(2) the probe payload is a 6-axis ambient cube, not N×N octahedral — blocky world,
6 texels/probe, trivial sampling.

## Data model

**Voxel window** — R8Uint 3D texture of GI-class ids, one texel per block, XZ-toroidal at
chunk granularity, Y spans the full 256-block column (an up-ray always reaches sky; the
window never scrolls vertically). Desktop 320×256×320 (25.6 MiB) / web 256×256×256
(16.8 MiB) / iOS 192×256×192 (9.4 MiB). GI-class mapping is a pure function in
`digblock-core` (headless-tested): air = 0, water = translucent, `emits`/`glows` blocks =
emissive classes, Shaped blocks = opaque (the same v1 approximation fluid makes). A 16³
chunk re-uploads as one 4 KiB `write_texture`; scrolling uploads only entering slabs.

**Material LUT** — 256 × 8 B uniform: albedo rgb + opacity class, emissive rgb + intensity
exponent. Albedo is the per-layer mean color, computed where the atlas upload already
iterates the layers. Rays never touch the block atlas.

**Coarse occupancy** — a second R8Uint 3D texture at /8 per axis; byte = any_solid |
any_emissive << 1 per 8³ region, computed CPU-side at classification (8 B/chunk). Rays in
empty space step 8 blocks per load.

**Light list** — ≤256 emitters within C1 range: pos, rgb, radius = 16 B each, one storage
buffer, rebuilt from per-chunk emitter lists kept at classification. Exists so next-event
estimation makes a single torch voxel stable at 32 rays/probe instead of a firefly.

**Probe cascades** — two toroidal camera-following grids of 6-axis irradiance probes:
C0 at 2-block spacing 32×24×32 (64×48×64 coverage), C1 at 8-block spacing 32×20×32
(256×160×256). iOS: 24×16×24 each. Payload: one rgba16float 3D atlas per cascade, dims
(nx, ny, nz×6) — axis-major Z slabs, hardware trilinear with a half-texel Z inset so
slabs never bleed. Ping-ponged: the update compute reads last frame's atlas (that read IS
the infinite bounce) and writes the new one; a full copy precedes the amortized write.
A r32uint flags texture per cascade carries age (8 b), dead-in-solid (1 b), and a
relocation code (5 b — probe center nudged ≤1 block into air). Beyond C1: analytic
fallback (ambient dome × face weight + lambert sun, visibility 1), faded in over C1's
last 16 blocks.

## Passes (frame order, RTGI on)

probe copy → probe update (compute, per cascade) → invalidate (compute) → depth prepass
(opaque solids, position-only) → depth blit to R32Float (existing pipeline, moved earlier;
one R32F serves both GI resolve and water) → GI resolve → scene → water → viewmodel →
overlay.

**Probe update** — workgroup 64. Spherical-Fibonacci ray set rotated by a per-update hash.
Per ray: coarse-skip DDA (~20 loads average) → on hit, LUT albedo/emissive + one sun DDA
from the hit + a 6-axis gather from last frame's atlas; on miss, sky radiance from the
frame's zenith/horizon gradient. Plus NEE against the ≤4 nearest list lights (one short
DDA each). Scheduling is a permuted round-robin over probe index — responsiveness comes
from the alpha ladder, not a priority structure.

**GI resolve** — fullscreen at half res (¼ iOS; web capped 960×540), MRT. World position
from R32F depth + inv_view_proj; face normal from depth derivatives (exact on a blocky
world). Per pixel: the 8 surrounding C0 probes — 3 axis fetches each (n² ambient-cube
weights) + a ~4-load DDA occlusion probe→pixel — falling through to C1 (occlusion capped
at 8 coarse steps) then analytic; plus one sun-visibility DDA (~20 loads). Out: rgba16f
(indirect rgb + view depth) and rgba8 (sun visibility + packed normal code).

**Scene shading** — chunk.wgsl: emissive bit-21 early return unchanged; else
`tex * ao * (light_color * sunvis * lambert + indirect)` via a 4-tap depth/normal-aware
bilateral upsample of the GI buffers. Water: manual probe gather at the surface world pos
in fs (no new varyings) + its own capped ~12-step sun DDA; underwater dimming is the sun
ray accumulating transmittance through translucent voxels. Players: per-fragment gather +
1-tap sun occlusion. Viewmodel/spray: a 1-invocation compute samples the field at the
camera into a 32 B buffer copied into their UBOs.

## Budgets and cadence

| tier | probes/frame | rays/probe | full sweep | resolve | GI upload/frame |
|---|---|---|---|---|---|
| desktop | 4096 | 64 | ~0.18 s | ½ res | 2 MiB |
| web | 1536 | 32 | ~0.5 s | ½ res, ≤960×540 | 256 KiB |
| iOS | 1024 | 32 | ~0.55 s | ¼ res | 512 KiB |

- Hysteresis alpha by probe age: 1.0 / 0.5 / 0.25 / 0.10 steady state.
- Invalidation: ≤16 CPU edit spheres per frame (edit ±8 blocks C0, ±16 C1) reset ages; a
  teleport zeroes every age and the alpha-1 sweep hides inside the streaming fill.
- Sun: ray direction quantized to ~1.5° (one step per ~5 s at DAY_SECS = 1200); steps do
  NOT invalidate — shadows glide across a step over 2–4 s of hysteresis. The smooth
  un-quantized direction still drives `light_color`/lambert.
- Convergence gates: a placed torch visibly lights ≤0.5 s desktop / ≤1.5 s web, settles
  ≤3 s.

## Invariants

- The voxel window is DERIVED data: never serialized, never on the wire; device loss
  rebuilds it by re-upload + age reset (drill-tested).
- Classification is a pure function of the cell value — headless tests pin it.
- Everything the rays read (window, LUT, coarse, light list, atlases) lives on the GPU;
  the CPU never reads probes back.
- Flag-off builds are bit-identical to the old system (P0 bench gate).

## Test pins (grown per phase)

- P1: classification table (air/water/emissive/shaped→opaque), coarse-byte derivation,
  window addressing (toroidal chunk→texel), dirty-set feed from set_cell/arrival/evict.
- P2/P3: naga-validated shaders (existing harness); frame-layout test updated with the GI
  uniform fields; capture A/Bs — DIG_VAULT (sealed room: dark, then torch-lit), DIG_ROOF
  (a 10am sun slants under a wall-less roof), tod 0/45/180.
- P5: bench with a fixed-tod converged steady-state segment.

## Phases

P0 compute infra + this doc + `rtgi` flag (gate: flag-off bench parity) · P1 voxel window
+ LUT + light list (gate: `DIG_GI=window` debug raymarch capture) · P2 probe cascades +
update/invalidate computes (gate: probe visualization capture) · P3 depth prepass + GI
resolve + chunk.wgsl (gate: A/B captures; USER LOOK SIGN-OFF) · P4 water/players/
viewmodel/spray/underwater · P5 perf + metrics + bench + web verify · P6 remove the old
system — DONE at M1 (shadow.rs, shadow.wgsl, core light.rs, mesher light plumbing, cascade
VPs, the ~26 pinned tests, LIGHT-DESIGN.md, the flag itself).
