# GI-DESIGN — probe-traced global illumination

Status: **M4 landed on `feat/rtgi`.** RTGI is the only lighting path and there is no flag.
The raster stack it replaced is DELETED, not dormant: shadow.rs/shadow.wgsl and the
cascades, core/light.rs and the Stage-B BFS voxel light field, baked per-vertex AO, all fog,
the hand-tuned water look, and per-block tint jitter. LIGHT-DESIGN.md went with them. The
tint is the one item on that list that came BACK (R23) — as an albedo property in
chunk.wgsl's `fs`, which is a different thing in a different place from the vertex-packed
step folded into AO that died here.

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
  itself does not use one. Disocclusion is rejected and then, where indirect is the light,
  PRIMED from the probe cache rather than filtered (R15).
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
- **The voxel window still covers half the streamed radius, and that is now a DECISION.** It
  is built `draw_dist` chunks on a SIDE (+/-160 blocks) while terrain streams to `draw_dist`
  chunks of RADIUS (+/-320), so 9.2% of pixels have a sun ray that leaves the window and is
  answered "fully lit" on no data, along a boundary centred on the camera.
  That statistic is real and it is very nearly INVISIBLE, which is the part that took
  measuring. Building a window that genuinely covers +/-320 (side 640 AND lifting
  `engine.rs`'s upload clamp, which is what the earlier attempt missed) and holding the sun
  ray's reach as the only variable moves the image by mean 0.0169 — at the 0.007 repeat-noise
  floor. Distant terrain is mostly lit anyway, so "lit" is usually the right answer, and by
  160+ blocks the participating medium has faded the surface most of the way into the sky.
  What DID matter at that range was the final gather, and it cost nothing to fix — see
  `gather_max` in gi_resolve.wgsl. With the gather fixed, the whole remaining benefit of a
  full-coverage window is mean 0.0556, confined to the horizon band, for **+75 MiB** (the
  window texture goes 25 -> 100 MiB; note `gpu_buffer_bytes` in the metrics counts mesh
  buffers only and does not see it) and a 4x slower initial fill, since the upload loop is
  byte-budgeted per frame. Declined on that trade.
  The earlier "doubling helps, mean 1.767, costs no frame time" note was **retracted**. That
  build could not have been measuring new data: `engine.rs` clamps the upload window to
  `draw_dist` independently of texture size, so the outer half of the texture was never
  written, and wgpu zero-initializes it — which `gi_cell` reads as air, the same answer the
  narrow window gave by breaking at its bound. Confirmed directly: with the debug view's
  `cam.w`-scaled depth cue neutralized so a diff means geometry alone, `DIG_GI=window` at
  side 320 vs side 640 differs by **50 pixels out of 5.4 million**. The 1.767 was several
  `side`-derived bounds moving at once against unchanged data, and most of it was
  `gather_max`.
  Whoever revisits this: the honest fix is not a bigger box. The window's Y extent is the
  full 256-block world height and most of it is empty air, so bounding Y is where the memory
  actually is; and the far field plausibly needs only coarse occupancy (80x32x80 = 205 KiB at
  side 640) rather than full material identity, since the gather ray only reaches
  `GI_GATHER_T` = 32 blocks.
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
- **The accumulator lives in SCREEN space, and that is a decision — the world-space
  alternative was considered and rejected.** Most published path-traced GI keeps its
  radiance cache on surfaces rather than on screen (Lumen's Surface Cache, NVIDIA SHaRC,
  EA SEED's GIBS/surfels), and the usual objection to doing it here does not apply: the
  surface parameterization problem that costs Lumen mesh cards and SHaRC a spatial hash is
  FREE in a voxel world, where a face is exactly `(cell, axis, sign)` and adjacent same-axis
  faces are contiguous, so bilinear works with no lightmap seams. The engine already has a
  toroidal GPU voxel window with that addressing. It would buy two things the screen
  accumulator cannot: convergence that SURVIVES motion (no reprojection at all, so nothing
  to smear and no travel cap), and a large cost saving, since every pixel currently traces a
  sun DDA every frame where SHaRC's update pass traces ~4% of them.
  It was rejected on **resolution**, which is a hard arithmetic wall rather than a matter of
  effort. The sun ray's penumbra 3 blocks from its occluder is ~0.05 blocks wide. An
  affordable dense cache is ~4x4 texels per block face (+/-32 blocks, 3 faces/cell, 2 B =
  25 MB), which quantizes that nearly-sharp edge into quarter-block stairs exactly where a
  player is looking; 16x16 resolves it but reaches only +/-16 blocks inside 50 MB. SHaRC's
  answer is a camera-distance-dependent cell size, which reintroduces re-bucketing at every
  level crossing — a per-frame reprojection problem in world space, which is the problem the
  whole exercise was meant to remove. Three further unsolved pieces sit behind that one:
  eviction as the window slides, sun-motion invalidation, and shading faces the camera has
  never seen.
  What made this a QUALITY question rather than a bug fix is that R9 already closed the
  defect that motivated it (walk residual 0.922 -> 0.578, flyover 2.84 -> 2.08, for 0.14 ms)
  using SVGF's spatial half instead. Indirect should stay on the probes regardless — it is
  smooth and does not smear — so the cache would only ever have carried sun visibility.
  Reopen only if "converged while walking" becomes a requirement; denoised-while-walking is
  what exists today.

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

**With one caveat, learned the hard way in R13: the warp half is not a fixed reference.** Any
change to the accumulator can move it too, and a change that shortens a STILL camera's history
blurs the reference toward the thing being measured — which reads as a large improvement and
is none. When the change under test touches accumulation, pin a converged frame once
(`--warp N --frames 3000`) and measure every variant against that. Low-pass the diff 8x8
while you are at it: a short history and a smeared one both differ from a converged frame,
and only the smear survives a box filter.

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
  reprojection is lossless. Two things taken as settled here were not, and R13 below corrects
  both: travel is NOT zero at rest, and a smooth `N_MAX/(1 + TRAVEL_K*travel)` is a per-frame
  RATE where the blur it bounds is not.

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

### R13 — the cap read the wrong variable, and rest was not rest

R9's bisect ran entirely on `--walk`, whose ground sits 3.5 blocks ahead. That is the only
part of the frame where per-frame parallax is large, so it is the only part where the travel
cap ever engaged; everything past about four blocks kept walking around near `N_MAX`. A
player reported it as a shadow ten blocks out that slides forward with them and snaps back on
approach, which is the exact shape of "the fix works within four blocks". Approaching does not
repair anything — the blur is roughly constant in screen texels, and walking closer magnifies
the feature past it.

Measured on `--walk`, history depth by distance band against the walked-vs-warped residual:

| band                | mean n | a-trous skipped | residual | repeat noise |
|---------------------|-------:|----------------:|---------:|-------------:|
| under 3 blocks      |     15 |            0.0% |    0.085 |        0.000 |
| 3-4 blocks          |     56 |            0.0% |    0.234 |        0.001 |
| 4 blocks to horizon |    195 |           23.3% |    0.633 |        0.005 |

n and the residual track each other monotonically, and the far field is doubly exposed:
`gi_atrous` skips any pixel at or above `N_CONVERGED = 256`, so a quarter of the band that
most needed the spatial filter was the band not getting it. Visualizing n directly — the
resolve already stores it in `sun.a` — shows it as a red cap over the horizon and near-black
under the feet.

**Rest is not rest.** The fetch round-trips a world position out through `inv_view_proj` and
back through `prev_view_proj`, and against a 640-block far plane f32 does not return what it
was given: a dead-still camera measures **0.055-0.11 texels** of drift (median 0.06 near,
0.07 far). That costs twice. The cap reads arithmetic as movement — and the bilinear fetch
mixes in neighbours every frame at n ~ 450, which compounds into a multi-texel streak
visible in a STILL frame. That streak was in every capture this project takes: the vault's
torch pool had two vertical bands running out of it, and the resting terrace faces were
mottled rather than flat. The earlier `TRAVEL_K = 1000` note cannot have measured what it was
read as; at 0.08 texels that setting caps n at 12, and a still camera's image cannot have
been 99% unchanged.

The fix is two lines and one idea: **snap the fetch onto the lattice when the miss is under
the arithmetic floor, then make the cap two-state.** `TRAVEL_EPS = 0.15` clears the measured
drift and stays under half the slowest real motion in the frame (the far field's 0.36 median
on `--walk`). Snapped means bilinear degenerates to a copy of the pixel's own history, so a
standing camera is exact and keeps all 512 frames; anything else falls to `N_MOVING`. There
is no useful middle, and the smooth form's mistake was pretending there was: once the fetch
is off-lattice the blur compounds at a rate the sub-texel PHASE sets rather than the speed,
because a pixel creeping a hundredth of a texel a frame still sweeps the whole phase range
across a 512-frame history. `a_moving_pixel_always_gets_the_spatial_filter` pins
`N_MOVING < N_CONVERGED` so the relationship the whole trade rests on cannot lapse again.

Measured against a common converged reference (`--warp 600 --frames 3000`), with the diff
low-passed 8x8 first — a short history and a smeared one both differ from a converged frame
and a raw mean cannot tell them apart, but Monte Carlo noise averages away under a box filter
and a displacement does not:

| far field, 4 blocks to horizon | shipped | R13    |
|--------------------------------|--------:|-------:|
| smear vs converged truth       |  0.9625 | 0.4993 |
| mean n walking                 |     195 |     27 |
| a-trous skipped walking        |   23.3% |   0.0% |
| mean n at rest                 |    ~450 |    510 |

The convergence floor — the same reference against a 600-frame still capture — is 0.2248, so
the excess falls 0.738 -> 0.275, a 63% cut. Cost is free: bench 152.8 -> 153.2 fps, GI trace
4.309 -> 4.312 ms, denoise 4.493 -> 4.495 ms. The four pinned stills all move (VAULT 2.21,
ROOF 3.48, SHAPES 3.41, DIVE 0.98) and all move the same way — the streak coming out.

**A false positive on the way, recorded because it is the trap this harness sets.** The first
form bounded `n * travel`, the total slide over a history's life, with no snap. It measured
walked-vs-warped 0.633 -> 0.236 in the far field and read as a clean win. It was not: with no
snap, the resting drift capped a STILL camera at n = 24 as well, so the reference had been
blurred to match the thing under test. `--fly` vs `--warp` compares against a reference the
change under test can move. Anything touching the accumulator has to be measured against a
FIXED converged frame, not against its own warp.

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

### R14 — the sun ray stopped being an estimate

R13's standing question dissolves once the right question is asked: the only reason sun
visibility needed a LONG screen history at all was that the penumbra was estimated with one
random cone sample per pixel per frame — a 1-spp Bernoulli trial that takes hundreds of
frames to converge. Every smear fix so far tuned how that history is carried; R14 removes
the need for it. This is the production shadow-denoiser pattern (NVIDIA SIGMA, AMD
FidelityFX): trace toward the sun's CENTER deterministically, record the occluder distance,
and reconstruct the penumbra SPATIALLY with a filter width of `occluder_t x tan(cone)`. The
reopen condition R13 set for the world-space cache — "converged while walking becomes a
requirement" — is met without building one, by making the signal cheap to converge instead
of expensive to carry.

The pieces, each load-bearing:

- **`gi_sun_vis` returns the occluder distance** — free, it is the DDA's own `r.t` at the
  blocked return. The resolve converts it to half-res texels through the screen derivatives
  (already computed unconditionally for Tint), so projection and grazing angle are both
  accounted for per pixel, with no new uniform plumbing.
- **The cone offset is STATIC, and anchored to the WORLD.** The offset cannot be dropped
  (the caster is planar and rational — dropping it is the lattice resonance), but it can
  stop being temporal noise: hashed from eighth-block world cells (the hash point pushed a
  sixteenth INTO the block behind the face, because a shading point sits exactly on the
  lattice plane the quantization cuts along), a world point asks the same question every
  frame FROM EVERY CAMERA. Screen-hashed was built first and shimmered: a moving camera
  slides every world point across the pixels, re-asking a different fixed question each
  frame — measured +0.08..0.16 consecutive-frame flicker in the mid terrain bands on the
  flyover, and mouse-look is exactly that motion at full screen speed. The world anchor is
  world-space accumulation's stability property without its storage.
- **Sun history is capped at `N_SUN = 12` frames, still or moving** — enough to antialias
  the dither, short enough that smear is bounded BY CONSTRUCTION (a 12-frame average can
  only be dragged 12 frames). Swept {8, 12, 16} on the shadow band: 1.395 / 1.339 / 1.426 —
  8 shows the dither through, 16 re-admits drag; 12 stands. Indirect keeps its own cap.
- **`n` moved into the sun target's g channel** (`code + 256*round(n)`, exact in f32's
  mantissa, pinned by `packed_metadata_stays_exact`) — WebGPU's baseline 32-byte attachment
  budget is spent, so the penumbra width earned sun.a by the pack. Every reader is a
  `textureLoad` masking `& 255u`.
- **`gi_atrous` gates per signal now.** Indirect keeps the original SVGF trade (filter
  below `N_CONVERGED`, skip above). The sun's filter is not denoising but RECONSTRUCTION,
  so it runs at rest too — a deterministic ray never builds a penumbra in time, and the old
  skip would have shipped stair-edged shadows on every still frame. Inside the penumbra
  regime there is deliberately NO luminance edge-stop (the 0/1 boundary is the thing to
  blur; pen -> 0 collapses the kernel to identity, so contact stays hard), the width itself
  spreads (max over accepted taps, so the LIT side of an edge blurs by iteration 2), and
  the sun taps' stride scales up to 4x for wide penumbras (`the_penumbra_fits_the_filter`
  pins PEN_MAX to the combined support). One shared tap ring serves indirect, Bernoulli
  sun and the spread — splitting them into three loops was built first and measured
  +0.32 ms; sharing the loads brought it back.
- **`N_MOVING` rose 24 -> 48, and R14 is what made that legal.** Pre-R14 the sun smear
  scaled with it and pinned it; with the sun on its own cap, deeper indirect history buys
  flicker without smear. The 1-spp gather's moving flicker is very visible in a torch-lit
  interior (DIG_VAULT on the walk path, consecutive-frame diff): 0.812 at 24 -> 0.693 at
  48 -> 0.632 at 96 — but 96 overshoots, the indirect's own resampling blur returns
  (shadow band 0.777 -> 1.133, non-shadow rows 0.607 -> 0.625). 48 is the sweet spot on
  both axes at once. Note the vault metric is IDENTICAL pre/post R14 at equal N_MOVING —
  that flicker predates R14 and was unmasked by the smear's removal, not caused.
- **Lighting edits invalidate screen history.** The probe pass already consumed <=4 edit
  spheres a frame; the same spheres now ride the resolve uniform for ~2 sweep periods and
  cap `n_hist` at `N_MOVING` inside them — a placed torch's pool lands in ~a quarter
  second instead of fading in over hundreds of frames on surfaces whose face code and
  depth never change. Continuous time-of-day drift gets the same property free from
  `N_SUN`; the `SUN_JUMP_COS` full reset stays for `/time`.

Measured on `--walk` against the fixed converged reference, shadow band (the rows where the
between-build delta lives — everything outside them is bit-identical across builds, which
is itself the cleanest proof the sun change touched only the sun):

| shadow band, walked vs converged  | pre-R14 | R14 (screen-hash) | R14 (world-anchor + 48) |
|-----------------------------------|--------:|------------------:|------------------------:|
| residual                          |  1.803  |            1.339  |                  0.777  |
| excess over the non-shadow floor  |  1.17   |            0.72   |                  0.17   |

At-rest frame-to-frame stability halved (0.133 -> 0.068 — the resting dither no longer
flickers at all, it is a constant). The four capture envs moved only in penumbra texture;
the vault (sun-free) is byte-for-byte the same lighting.

**Measurement traps this round added to the list:** a seedless capture rolls a NEW WORLD
(two "repeat" runs once compared two different planets); the walk pair needs `--tod` frozen
or the 3000-frame reference sits under a different sun than the walked run and the diff
measures the sky; `--frames` is the TOTAL frame the screenshot fires at (`--fly 600` wants
`--frames 601`, not 1); `--tod` is dev-profile-only; and consecutive `just bench` runs
drift thermally by more than the effects under test — only interleaved brackets are
comparable, and the flyover's consecutive-frame diff cannot see rotation-only artifacts
because its translation term dominates.

### R15 — a reveal starts from the cache, not from noise

The reported artifact: in indirectly-lit places — in shadow under a cliff, in a cave —
surfaces the camera reveals (looking down a cave mouth, strafing or backpedalling so new
world enters at the screen edge) "ghost" for a split second before settling. The mechanism
is the disocclusion path: a pixel with no valid history restarted at n = 1 from its own raw
1-spp gather, and where indirect light is the WHOLE signal that single ray is speckle — a
cave floor's ray mostly finds dark walls and occasionally the bright mouth — so the
revealed region visibly developed while n climbed through N_MOVING. Sun-lit reveals never
showed it because the deterministic sun ray is converged from its first frame.

The fix is the one the literature converges on (AMD GI-1.0's hash-grid fallback, Lumen's
world-probe fallback at failed reprojections, NVIDIA's irradiance-cache disocclusion mix;
ReBLUR's "history fix" pass is the same idea built by someone with no world cache to read):
answer the no-history pixel from the world-space cache. This engine already owns one — the
probe cascades, converged and resident — so a disoccluded pixel now primes its accumulator
with the cascade blend at `N_PRIOR = 8` virtual samples and lets the ordinary 1/n machinery
decay the prime out. No new buffers, no per-frame blend: the prime is an initial condition,
and its residual weight after k real samples is ~8/(8+k).

Two gates, both measured in rather than assumed, because the first cut fired on every miss
and was a 9x regression:

- **A grazing miss is not a reveal.** The near ground fails all four history taps by a
  hair on most walking frames (adjacent half-res texels on a grazing plane legitimately
  differ by more than the 2% depth tolerance), and the walk's backward half reveals the
  whole bottom screen edge every cycle. Primed indiscriminately, walked-vs-converged went
  0.69 -> 6.3 with the bottom third at 13.1, linear in N_PRIOR — the near field locked at
  cache lighting, which is exactly the contact darkening the cache cannot see. So a
  same-face tap that missed depth by ≤ `PRIME_MARGIN` (8) tolerances testifies the surface
  never left the screen, and the pixel keeps the old unbiased n = 1 restart. A true
  reveal's tap held the OCCLUDER, whole blocks nearer — different in kind, not degree.
- **Only where indirect is the light.** A distance ramp was tried first (no prime under 4
  blocks, full by 20) and just moved the bias to whatever band the ramp allowed — the
  flyover's 12-25-block ground came back at 4.33 against 1.41. The partition that works is
  the sun: where `sun_vis x N·L x above_horizon` clears `PRIME_SUN_MAX` (0.2) the gather is
  a minor correction and the prime only pollutes it; where it doesn't — shadow, cave,
  night, exactly where the ghost lives — the prime runs at full strength.

Gated, the sun-lit regimes return to baseline (walk 0.68 vs 0.69, and the flyover's
consecutive-frame flicker actually improves, 0.215 -> 0.197, because cache values are
temporally stable where speckle is not), and the target case moves: DIG_VAULT under
`--walk`, the torch-lit interior where every reveal is indirect, drops its walk flicker
0.443 -> 0.332 (bottom third 0.462 -> 0.220, half) and its walked-vs-converged from 2.90 to
2.61 with the wall band 4.16 -> 3.06. The flyover's walked-vs-converged rises 1.41 -> 1.83
in its shadowed distant bands and that is the intended trade read by a metric that counts
bias but cannot see variance: those bands now show smooth cache lighting for the ~8 frames
a reveal used to spend as speckle. The vault's converged still is unchanged (0.07).

Two follow-ups from the first play test, same day:

- **The prior is `gi_cache_at`, not `gi_indirect`.** Priming is bursty by nature — a camera
  swing into a cave mouth reveals a hundred thousand half-res pixels in ONE frame — and the
  full cascade blend is ~112 loads per call (8 probes × relocation walk × occlusion DDA,
  twice), a call class already measured at 107 -> 69 fps when it ran per-pixel. That burst
  was a visible frame spike on every cave look-in. The 4-load nearest-probe lookup that
  already terminates every gather path is plenty for a prior that decays out in N_PRIOR
  frames.
- **The history depth tolerance widens to the surface's own gradient — but only on a
  total strict miss** (SVGF's `fwidth(z)` rule, the one this system had skipped): a flat 2%
  rejected every tap on every frame for any surface seen nearly edge-on — an east face
  viewed looking north — because adjacent half-res texels on a grazing plane legitimately
  span more depth than that. Those surfaces churned at n = 1 as a distracting flicker band
  precisely where a face turns away. The tap loop now runs strict-first: pass 0 is the old
  flat tolerance byte for byte, and only when it comes up EMPTY with a same-face near-miss
  on record does a second pass rerun with the tolerance widened to `GRAZE_K` (2) texels of
  the pixel's own per-texel depth span (`texel_world`, already live for the penumbra),
  capped at `PRIME_MARGIN` base tolerances — sharing the reveal threshold, so a silhouette
  gap stays rejected and the graze/reveal classification cannot disagree with the
  acceptance rule. An always-widened single pass was built first and measured +0.2-0.4 ms
  on the bench for the whole screen's benefit of nobody; the two-pass form confines every
  new instruction to the churn population. Measured (always-widened variant; the two-pass
  accepts a superset of nothing it accepted): the walk's smear guard IMPROVED
  (walked-vs-conv 0.69 -> 0.66 — the walk translates toward/away, so its taps already
  passed; rotation past an edge-on face is the case no harness path exercises, which is
  exactly why the artifact survived this long), vault moving flicker 1.21 -> 1.15, and the
  vault's walked-vs-converged gave back R15's earlier gain to sit at parity with pre-R15
  (2.92 vs 2.90) — the deliberate trade of churn for a mild motion-blur that recovers at
  rest, plus a prior that is coarser by four loads.

### R16 — a slab shadows as a slab

Shaped cells classified OPAQUE in the voxel window ("the same v1 approximation fluid
makes"), so a 1/3 slab cast a full-block shadow onto the slab beside it, a torch post
shaded like a pillar, and an open door blocked its own doorway. The literature check
turned up the same artifact and the same fix shipped in SEUS PTGI ("blocks which are not a
full-sized cube... aren't considered properly in lighting" -> per-shape sub-box tests
behind a quality toggle) and formalized as ESVO's per-voxel "contours" (Laine & Karras
2010): keep the grid, and clip the ray's in-cell [t_entry, t_exit] span against sub-cell
geometry stored per class. The alternatives — fractional opacity per cell (VXGI-style,
position-blind: a grazing ray that clears the slab's top still attenuates) and finer
voxelization (Teardown, memory x resolution³) — lose to it on exactly the deterministic
sun ray this engine leans on.

Classification is state-aware now: shaped cells map per (id, state) to synthetic classes
from `SHAPE_CLASS_BASE` (200) up — 25 of them — each carrying a ≤2-box cover of the
shape's `shapes.rs` collision boxes in the widened LUT (box 0 = collision box 0 exactly,
box 1 = the AABB union of the rest; exact for slabs and torches, a stair's L rounds up
only its top rear quarter; over-blocks only, never under, worst case the old full cell).
A state with no collision — an open door — classifies as AIR, which is what lets light
through a doorway. `gi_boxes_hit` lives in gi_dda.wgsl so all three marches (the resolve's
sun ray, the gather, the probe kernel's trace) share one definition and a slab shadows the
cache exactly as it shadows the per-pixel ray; `gi_solid`/`solid_cell` (probe placement,
the short probe-visibility DDA) deliberately still treat partial cells as whole — the old
conservative behavior, one fewer thing to re-verify.

One rule was earned by a broken capture rather than designed: **an emitting partial cell
is a hit whole for the gather marches.** The first cut let gather rays sail through the
torch's cell unless they clipped the thin post boxes, and the vault's pool collapsed
(old-vs-new 12.5) — LIGHT_GAIN is calibrated so the direct-hit term `alb x emit x 0.6`
matches the NEE pool, and the voxel IS the emission model. With the rule, the vault's
converged still is back to 0.07 old-vs-new. Only the sun ray resolves an emitter's boxes:
a torch must not cast a pillar shadow, but it must still glow.

DIG_SHAPES photographs the payoff: the showcase's row of identical full dark squares
resolves into a stepped stair profile, thin slab bands, and nothing at all under the open
doors. Terrain captures move by noise only (conv 0.0075-0.03) — worldgen places no shaped
cells, so the box test's cost is confined to player-built cells.

### R17 — water stopped paying per pixel for a signal that varies per probe

Standing at a river's edge looking down halved the frame rate, with the loss swinging as
the view moved. The cause was priced into the code from M2 and documented next to
`gi_cache_at` without anyone adding the two numbers together: water shades its indirect
term by calling `gi_indirect` — the full 8-probe cascade blend, ~112 texture loads with a
relocation walk and an occlusion DDA per probe — **per fragment, at full resolution**. The
resolve survives that blend only because it runs at half res with temporal history behind
it; the comment beside `gi_cache_at` even records the measurement (107 → 69 fps when the
full blend ran per half-res pixel). And the cost is view-dependent by construction:
`gi_cascade_gather` early-outs to the analytic dome outside cascade coverage, so an ocean
on the horizon was nearly free while a river underfoot — every fragment inside C0, beside
bank geometry that runs the relocation walks to their full six steps — was the worst case
for the whole screen at once. That coverage cliff is exactly the "spikes when I look down"
report.

The fix is where the signal's own frequency says it should be: **the gather moved to the
vertex shader**. Water quads are strictly per-cell (`emit_water_face` — corner drop codes
are per-cell state, so nothing merges), which puts vertices ≤1 block apart against a probe
field that is 2-block spaced and smooth by construction; perspective interpolation across
a quad cannot lose anything the blend could represent. Millions of per-fragment blends
become thousands of per-vertex ones, and the coverage early-outs keep distant vertices at
pure ALU. The per-fragment sun DDA (SUN_STEPS = 12) stays per pixel — shadow edges on the
surface are the one lighting signal sharper than the lattice, and it was never the cost.

Two structural notes. The depth prepass gets its own position-only `vs_depth` entry so it
doesn't pay the gather for geometry it only depth-tests; both entries build their
`@invariant` position from the one shared `water_world` decode, which is what the color
pass's LessEqual-against-prepass depth test demands (a divergence here is water full of
view-dependent holes — the failure mode is unmissable, and the sea capture would have
screamed). And the gather bind group (`gather_bgl`) gains VERTEX visibility — WebGPU
baseline allows read-only storage and sampled textures in the vertex stage, so this fits
`Limits::default()` unchanged.

Parity: the sea-view still (seed 7, tod 45, spawn pose — open water right of frame) moved
0.0504 old-vs-new against a 0.0435 same-build repeat floor (animated ripples dominate both
numbers); the bottom-third band, all terrain, read 0.0001. Players (`players.wgsl`) keep
the per-fragment gather deliberately: a player model is a few thousand fragments, not half
the screen.

**The honest postscript (R18's measurement):** this fix was real but it was a THIRD of the
water pass, not the whole story — old→new measured 3.8 → 2.5 ms at a small window, and the
user's play test correctly reported "no improvement." What the first diagnosis missed is in
R18.

### R18 — the sun march learned where the sky starts

The user's report survived R17, so the guessing stopped and the scene got a harness pose:
`DIG_RIVER=1` excavates a 49×49, 7-deep flooded basin ahead of the spawn camera (pair with
`--pitch -35`), because NO existing capture put a shaded water surface on screen — DIVE is
submerged, the spawn sea is 100+ blocks out, and that gap is exactly where R17's "fixed it"
came from. Bisecting on that rig at the user's window size (Retina ~9 MP; `--size` holds
the logical size but the physical size follows the display — verify the capture's pixel
dimensions before comparing runs, two of these runs silently landed on different backing
scales):

- Stubbing the SSR march: −0.8 ms. Not the cost — looking down, the reflected ray exits
  the screen in a few steps.
- Stubbing the water sun DDA: −4.2 ms, the ENTIRE river-over-terrain premium. Twelve
  iterations at 8 blocks per brick skip spent proving "open sky" — the loop has no early
  exit short of y = 255, so every one of ~5 M water fragments paid ~9 dependent texture
  loads to learn nothing was there.

Two fixes were built; ONE survived measurement. The 64-cube super-brick (coarse mip 3,
`gi_ray_skip_box`) lost both ways it was wired: checked before the brick it taxed every
dense iteration of a latency-bound loop (river +5 ms), nested behind the brick it taxed
every empty one, and at terrain altitudes a 64-cube almost always holds SOMETHING — the
world's occupancy lives in a thin shell around y 120–220, so a cubic skip level asks the
wrong-shaped question. Deleted, not shipped. What shipped is the **sky ceiling**: the
window's highest occupied cell y, scanned on the CPU from a 51 KiB coarse-occupancy mirror
whenever a chunk classifies (`sky_ceiling` in gi.rs), carried in `frame.gi_misc.y` /
`rz.jitter.z` / probe `zenith.w`, and tested per iteration in every march as one float
compare: a non-descending ray above it is a proven miss/lit. The climb now ends at the
tallest local hill instead of the top of the world.

Measured at matched 5.4 MP windows, settled intervals: river 14.5 → 13.2 ms, terrain-only
control 10.4 → 9.2 ms — the ceiling pays everywhere (every resolve sun ray and probe ray
climbs), not just on water. Parity: dive (underwater sun transmittance) 0.0125 ≈ floor;
sea-view terrain band 0.0005; the sea's water band moved 0.10 entirely inside the distant
glint stripes, which is ripple PHASE — the animation clock reaches frame 1200 at different
wall times on different builds, a capture-harness trap to remember alongside the seedless
one. The remaining river-over-terrain premium is ~4 ms at 5.4 MP: the sun DDA's surviving
iterations near banks plus water's ordinary per-fragment work (ripples, refraction,
medium). The next lever if it's ever needed is a per-column max-height texture (a real
heightfield DDA answers "how far up until sky" in one load where the cubic mip could not);
it was scoped and deliberately not built — diminishing returns until a player reports it.

### R19 — the column field: built, measured, declined

R18's scoped next lever got its trial and did not ship; the full patch is parked at the
`archive/r19-column-skip` tag. What was built matched the scoping exactly — a per-column
max over every NONZERO class (water included, since the field licenses SKIPPING and the
coarse bit-0 rule would let a jump lose the pool `sv.y` counts), merged per chunk into a
side×side R8Uint field, and a `gi_ray_skip_column` that jumps a dense column
bit-identically to stepping it. Measurement then cut it to one call site: with the gate
in BOTH gi_common marches the resolve's trace cost went UP (river pose +1.1 ms), because
an underwater pixel's sun ray sits below the water-inclusive colmax for its entire climb
and pays the dependent 2D load per dense iteration without ever skipping — the
super-brick lesson in a new hat. The one ray that profits is water.wgsl's `SUN_STEPS =
12` sun march, launched at its own column's waterline; even there the gain was
correctness, not milliseconds (frame cost inside bracket noise on river/shore/terrain at
4.1 MP): the 12-iteration budget TRUNCATES into "lit" against far banks (`gi_sun_vis`
answers lit when it runs out, measured at 0.10–0.23 low-passed in the water bands
against an un-truncatable 96-iteration build), and the skip let the same 12 iterations
reach the real occluder. That look change was declined at sign-off, so the truncation
stands as a KNOWN, accepted artifact: distant shorelines render slightly un-shadowed.
If a player ever reports it, start from the parked branch, not from scratch.

Two harness traps earned their entry this round: **physics ticks ride the wall clock**,
so two builds of different speed reach `--frames N` with the spawn settle in different
states and every cross-build still is polluted by a sub-block camera shift — capture at
≥1200 frames so both builds are long settled (terrain parity then reads 0.028 vs a
0.014 repeat floor); and a coherent reflection streak shifted by ripple phase does NOT
cancel under a low-pass, so water-band diffs across builds need eyes on crops, not
thresholds.

### R20 — torch light became a shadow ray, and the support block lit up

Two symptoms, one estimator. The user's "massive flicker moving near a torch" and
"torches should light the block they sit on" are the same defect seen twice: per pixel,
torch light arrived ONLY when the 1-spp gather ray geometrically struck the torch's cell
(`alb * emit * LIGHT_GAIN`, ~2% odds at 2 blocks, re-rolled every frame) — a
high-magnitude Bernoulli trial history tuning was already measured unable to drown (the
N_MOVING sweep at R14). And the support block could never win even the lottery: every
march steps before it tests, so a ray leaving the top face starts INSIDE the torch cell
and exits unregistered. The brightest surface in the room structurally received nothing.

The fix is the R14 move applied to local lights (Q2RTX / GI-1.0 / RTXDI lineage — direct
light gets its own deterministic low-variance estimator): the resolve binds the same
`lights` buffer the probe pass NEE-samples (binding 5, its second fragment storage
buffer), picks the DOMINANT in-range light per pixel (max `emit·ndl/max(d²,1)`, a
deterministic pick so the pool is steady at n = 1), fires ONE `gi_march` shadow ray
stopped 0.9 short of the light's cell, and adds the probe `nee()` formula minus the
×count pdf. The gather's emission term for that one cell is SUPPRESSED whether or not
the shadow ray connected (`path_radiance`'s `skip_cell`) — half-suppression puts the
flicker back in the torch's shadow; other emitters keep the direct-hit term since
nothing NEE-samples them. The disocclusion prime adds the (already computed) NEE term so
a cave-mouth reveal beside a torch starts at the pool every later sample votes for.
`LIGHT_RANGE`/`LIGHT_GAIN` are const twins of gi_probe's, pinned by
`resolve_and_probe_agree_on_light_calibration` (which required teaching the test
helper WGSL's typed const form — the untyped pattern reported gi_probe's as undeclared).

Measured on the vault rig: the under-torch dark patch is gone; the walk's
consecutive-frame flicker in the bright pool band halved (2.44 → 1.10) and the mid band
dropped 35% (0.857 → 0.558) — the remaining whole-frame delta is texture-edge resample
shimmer under motion, visible as the wall speckle pattern in the diff heatmap, not
lighting noise. The room is legitimately brighter overall (+7.8 mean): per-pixel direct
light used to be under-counted by the hit-lottery model everywhere the probes' point-lit
cache couldn't reach except as bounce, and the ray now prices a torch exactly as the
cache does — which is also what removed the near/far seam risk. Torch shadows are hard
(point light, no penumbra machinery — v1, judged at look sign-off; the pool level moved
and LIGHT_GAIN retune-in-both-files is the sanctioned knob if it reads too hot). Cost:
+0.3–0.6 ms trace at 4.1 MP in the sealed vault (every pixel in range of the torch — the
worst case; brackets overlap same-build spread), zero measurable in torch-free scenes
(count == 0 is one header read), frame time display-paced and unmoved. The light list
also refreshes on >16 blocks of camera movement when more than MAX_LIGHTS emitters are
resident — order never mattered to a reader, but the truncation to the nearest 256 was
frozen at whatever camera last saw a chunk classify.

### R21 — water was marching two rays whose answers were multiplied by nothing

Third attempt at "looking into water halves the frame rate", and the first one that
measured before it changed anything. R17 moved water's indirect gather to the vertex
shader; R18 shipped the sky ceiling; R19 built the column field and was declined. The
user's verdict after all three was that the cost was still there — and this time they
gave the number that reframed it: same spot, same draw distance, **no water in view 120
fps, lake in view 50.8 fps**. Water was not a 4 ms premium, it was HALF THE FRAME.

Two things had to be built before a single line of shading changed, because both prior
rounds had been reasoned from source and measured on a pose that could not show what they
moved:

- **`DIG_STUB`** (`renderer::stub`) — a uniform-branch mask that removes one shading term
  per run, so a single build attributes a whole frame. Uniform, so an un-stubbed run pays
  nothing and a stubbed one cannot diverge inside a wave.
- **`--pos x y z` + `--fixed-dt S`** — stand in real worldgen water rather than in
  `debug_river_plan`'s flat-walled box (a real river is TERRACED: the sim's 8→4→2→1 spread
  gives every step its own level and its own unmergeable per-cell quad, the whole bed is
  bank rather than just the rim, and cascades make impact cells the box has none of), and
  step the world by a constant so two builds reach frame N in the same world state.
  `--fixed-dt` alone took the DIG_RIVER same-pose repeat floor from **0.69/255 (max 124)
  to 0.0001 (max 1)**: everything animated rides dt, so a cross-build capture used to
  measure the speed difference, in the ripple bands, at ten times the floor. That is R19's
  two harness traps retired rather than documented again.

The attribution, at 5.4 MP / draw 32 / seed 42, submerged looking up at the surface —
the "it's slow underwater too" case that killed every fragment-count hypothesis:

| stub | avg ms | saved | share of water |
|---|---|---|---|
| none | 14.85 | — | — |
| `wsun` (12-iter per-fragment sun DDA) | 10.93 | **3.91** | 55% |
| `wssr` (28-step screen-space march) | 13.16 | 1.68 | 23% |
| `wvert` (per-vertex `gi_indirect`) | 13.93 | 0.91 | 13% |
| `wripple` / `wrefract` / `spray` | ~14.8 | ≤0.13 | noise |
| `wflat` (whole fragment shader) | 8.52 | 6.32 | 88% |
| `nowater` | 7.69 | **7.15** | 100% |

Repeat floor 0.02 ms. Water was 48% of the frame and the no-water frame ran at 130 fps —
the user's "120 at times", reproduced.

**Then the finding that mattered: both dominant terms were being multiplied by ~zero.**
Water has no diffuse sun response — a transparent surface's colour is the refracted
backdrop plus the reflection, and neither uses `sh`. Sun visibility reaches the pixel ONLY
through foam (aeration, the shore line, splash cells), the specular glint and the lip
sheen. On calm open water away from the glint, and on ANY surface seen from below, every
one of those weights is ~0. Meanwhile `reflection_color` is called as a FUNCTION ARGUMENT,
so its 28 dependent depth loads ran even where `mirror_w` is pinned to exactly 0 (the
underside of a surface, submerged). So both marches are now gated on their own
contribution: `sun_w` (the max of those five weights, times the light colour) against
`SUN_MIN_L`, and the mirror march against a mirror weight of exactly zero. This is
importance, not approximation — the admitted error is the weight times the radiance the
march would have returned.

The threshold has to be in RADIANCE, and that is a lesson the parity pass taught: the
first cut compared bare blend weights, and a 2% Fresnel weight against an HDR sky (this
engine has no tonemap, so 1.0 is merely "white") put a ripple-banded difference across a
whole basin at mean 6.96/255. Weighting by `light_color` and dropping the mirror
tolerance to exactly zero fixed it.

Measured after, same poses: water **7.15 → 1.40 ms** submerged-looking-up (frame 14.85 →
9.09, 67 → 110 fps), **5.22 → 1.20** submerged-flat, **2.45 → 1.60** above water at a
terraced river with a cascade. Parity with `--fixed-dt`: DIG_RIVER at −35 and −60, the
natural cascade pose, the submerged pose and DIG_DIVE all diff at **mean ≤0.0023, max 1**
(one pose has max 9 on isolated pixels, 0.000% over 2). The two ray marches that were 78%
of water are now ~7% of it; the biggest term left is the per-vertex `gi_indirect` at 0.83
ms, which is the measured next lever if water is ever the problem again.

### R22 — the torch out-lit its own flame

User report: "the bright kind of overexposed looking circle right under torches", and a
follow-up that the flame itself should read brighter. Both are the same defect, and it is a
DISPLAY-RANGE one — which is worth naming, because the honest fix is the thing this engine
deliberately does not have.

R20's NEE prices a torch as a point source: `emit(14) × LIGHT_GAIN(0.6) × ndl / max(d², 1)`.
The support block's top face sits half a block from the torch's centre, so it took the whole
8.4 against a display whose white is 1.0 — a hard-edged ellipse of saturated orange with the
floor texture gone inside it. Measured in the vault band: **2.96% of pixels with a channel at
255 and not one of them actually white**, which is the giveaway — this is per-channel
saturation shifting the hue to the light's, not a bright surface.

Two changes, both stopgaps for the absent tonemap and both explicitly labelled as such:

- **`LIGHT_NEAR = 2.0`**, the pinned twin of the existing near-field clamp (which was 1.0 for
  the divide-by-nothing case, and is now also what keeps the near field near the white point).
  Outside the radius the falloff is exactly 1/d², so nothing past it changes at all — the
  floor and wall bands away from the torch move under 0.5%, and an outdoor scene with no
  emitters diffs at **mean 0.0004, max 1**. The radius is a measured trade, not an extreme:
  clipping goes 2.96% → 0.40% at 2.0 and bottoms out at 0.13% by 2.5, but that 0.13% IS the
  torch's own emissive sprite (2.5 and 3.0 measure identically), and buying the last 0.27%
  costs another 14% of pool brightness on a pool that already reads faint. What is left at
  2.0 is a hot core at the flame's base; what went away is the flat ellipse.
- **An emissive-ink gain in chunk.wgsl**, riding the ink's OWN luminance
  (`1 + 1.6·lum²`). A light source has to out-read what it casts or the picture inverts —
  and once the pool stopped clipping, the flame was dimmer than the light it threw. The
  weighting matters because a torch is one block but not one material: head and wooden post
  are both fullbright, and a flat multiplier lifts the post into glowing wood. This is the
  ink alone; `glows` and `emits` stay independent, so nothing here changes what a torch CASTS.

Neither touches perf (the clamp is a compile-time constant; the gain is a dot product on a
fragment that already does eight texture loads). **The real fix stays on the backlog:** an
HDR scene target plus one tonemap on the composite would roll the highlight off instead of
flattening it. Note the offscreen scene colour is created with the SURFACE format today, so
radiance is already clamped at the scene pass — a tonemap cannot simply be appended, it needs
the HDR target first.

### R23 — the per-block tint came back, on the albedo this time

User report: the pre-RTGI engine had a per-block tint that broke up large flat surfaces of a
single block type, and the flat RTGI look wants it back. It died at the M1 demolition
(3cbed02) as collateral, not by argument: the old one (b6af702) packed a 3-bit splitmix hash
of the world block coords into vertex bits 29-31 and folded it into the AO varying, so when
AO went and the vertex words were repacked tight, the tint had nowhere to live.

It does not need a vertex bit now. `fs` already carries `i.world` and the flat face code, so
the block cell is `floor(world − n/24)` — nudged half a twelfth along the face's own normal,
because on the face's axis the world position sits exactly on a cell boundary where `floor`
answers the neighbour. That is exact for full blocks and shaped sub-boxes alike (the nudge is
finer than the thinnest sub-box), so: **no vertex-format change, no re-mesh, no mesher touch,
and no new varying** — which also means the `@invariant` contract with the depth prepass is
untouched.

**It multiplies albedo, never light.** That placement is the load-bearing part, and it is a
placement the pre-RTGI engine did not have to think about: the à-trous filter and the temporal
accumulator only ever see the lighting buffers, so a per-block step written into the light is
a signal they are built to smooth across exactly the edges it lives on — blurred toward its
own mean at stride 1, then averaged over up to 512 frames of history. On the albedo it lands
after the bilateral upsample, where nothing downstream can filter, blur or accumulate it.
Emissive ink is excluded structurally rather than by a test: the tint is computed inside the
`i.emissive == 0u` branch, so an authored flame never reaches it, and R22's `ink`-weighted
gain cannot be detuned by a ±8% albedo underneath it.

Calibration was a four-way sweep (0, ±4%, ±8%, ±12% value with hue at 5/8 of it) captured on
the DIG_VAULT stone wall, which is the flat one-material surface the feature exists for.
**±4% is invisible**, and that is worth recording because it is the old jitter's ×0.95..1.05:
mean |diff| 1.62/255 over a daylit terrain frame, max 10, and a 148-patch sample of the lit
ground plane moved its luma stdev 28.61 → 28.65 — inside the noise. ±12% quilts: the wall
stops reading as one material. **±8% value ships**, straight off the sweep. The **hue ships at
±3%**, dialled back from the sweep's coupled ±5% on a live look — the two knobs are independent
by construction (the hue rotation moves luminance by only 0.185·d) and the sweep only ever
varied them together, so the value number is measured and the hue number is a look call on top
of it. Note the sweep had to be shot in the dim room rather than in daylight, because an albedo
jitter scales with the light — it varies most where a surface is brightest and all but vanishes
in shade, which is correct for a material property but means a daylit capture flatters it.

Two things the sweep also settled: the hash is a lowbias32 avalanche over a per-axis odd-stride
sum of WORLD coords, and a 16× amplified diff shows crisp per-cell patches with no plane
correlation along any axis and no 16-block tiling — chunk-local coords would tile, which is
the same reason the old CPU hash took world coords. And the probes' material albedo stays
untinted, so a block's ±8% does not ride its own bounce light: below the noise floor of a
1-spp gather, and paying for it would mean a per-cell lookup inside every march.

### R25 — water wrote itself over the backdrop it had already absorbed

User report, with a screenshot: past some distance "the blocks under the water lose their blue
colouring and look grey instead". Reproduced exactly from the debug panel's new `pos` line
(`--seed 42 --pos 95.5 132.5 85.5 --yaw -313.7 --pitch -17.7 --tod 25.7`), which is what that
line is for.

Four candidates went first, each killed by an A/B rather than by argument:

| suspect | test | result |
|---|---|---|
| `thick` wrong at range | visualised it; CPU ray-march of the same pose | 176–200 blocks, march says 181 — **correct** |
| SSR reflecting the seabed | `DIG_STUB=wssr` | ≤4/255, no hue shift |
| aerial haze | `AIR_VISIBILITY` 1600 → 1e8 | ≤12/255 on submerged pixels, r/b moves 0.06 |
| Fresnel sky reflection | forced `mirror_w = 0` | band gets dimmer, **not bluer** (r/b 0.55 → 0.57) |

Then the decomposition. `backdrop_col` — the scene sample with the medium applied — has a red
channel of **exactly zero** across the whole band, which is right: 181 blocks of water at
`WATER_SIGMA.r` is `exp(−65)`. Yet the final pixel's red was 53/255, and `back + surf` could
not account for it. The missing term was outside the shader.

`renderer.rs` wrote a second copy of the frame UBO for the water pass differing in one field,
`misc[0] = 0.7`, commented "Java transparent-pass u_Alpha" — and water's alpha was built on it.
That constant is from the raster port, and it outlived the M1 demolition that replaced the
absorption model justifying it. The point of the current model is that the water fragment
**samples the scene itself** and runs the transport over it: `back + surf` already IS the
backdrop, absorbed. Blending that over the same backdrop at 0.75 re-adds an unabsorbed copy of
it at 25%.

Near shore the leak is invisible — a foot of water barely changes the bottom's colour, so the
absorbed copy and the raw one nearly agree, which is why this survived every shallow-water
capture the engine has. Under 180 blocks of water it was the entire picture: the transport
takes the backdrop to black, and what you see is a quarter of the raw seabed in its own grey,
against nothing. Hence "blocks under the water" — they were literally the blocks, arriving by a
path the model does not have.

Alpha is now the falling-sheet streak gate alone (`clamp(0.75 + 0.5·s)`, and `s` is 0.5 on
every still surface, so exactly 1.0 everywhere else). A torn sheet genuinely is see-through in
its gaps and `thick`, reconstructed from the depth buffer, cannot know about the tear — that
one stays. With `misc.x` unread by water, the second frame UBO and its per-frame upload went
too; the water bind group binds the same buffer the solids do (it still needs its own bind
*group*, for the resize-recreated scene targets).

Measured at the reported pose: the grey band's r/b goes 0.55–0.58 → 0.20–0.24, the shallows
move 0.10 → 0.06, mean |diff| 18.0/255 over the water region and 0 outside it. DIG_RIVER and
DIG_DIVE re-shot for sheets, shore lines and the submerged path.

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
emissive classes. Shaped cells were "opaque, the same v1 approximation fluid makes" until
R16: they now classify per (id, state) into synthetic PARTIAL classes from
`SHAPE_CLASS_BASE` (200) up, each carrying a ≤2-box cover of the shape's collision boxes
(box 0 = the first collision box exactly, box 1 = the AABB union of the rest — over-blocks
only, never under, worst case the old full cell), and a state with no collision at all (an
open door) classifies as air, which is what lets light through a doorway. A 16³ chunk
re-uploads as one 4 KiB `write_texture`; scrolling uploads only entering slabs.

**Material LUT** — 256 × 16 B storage: albedo rgb + opacity class (0 air / 1 water /
2 opaque / 3 partial) and the cast-light level in the first two words, the partial class's
two packed sub-boxes (twelfths, 4 bits per coordinate; 0 = no box) in the last two.
Albedo is the per-layer mean color, computed where the atlas upload already iterates the
layers; synthetic shape classes borrow their base block's albedo and light. Rays never
touch the block atlas. `gi_boxes_hit` (gi_dda.wgsl, so the probe kernel and the fragment
marches share one definition) tests the boxes against the ray's in-cell [t_entry, t_exit]
span — the ESVO-contour / SEUS-PTGI pattern.

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
