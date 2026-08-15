# GI-DESIGN — probe-traced global illumination

Status: **M4 landed on `feat/rtgi`.** RTGI is the only lighting path and there is no flag. The raster stack it replaced
is DELETED, not dormant: shadow.rs/shadow.wgsl and the cascades, core/light.rs and the Stage-B BFS voxel light field,
baked per-vertex AO, all fog, the hand-tuned water look, and per-block tint jitter. LIGHT-DESIGN.md went with them. The
tint is the one item on that list that came BACK (R23) — as an albedo property in chunk.wgsl's `fs`, which is a
different thing in a different place from the vertex-packed step folded into AO that died here.

What lights the world now, in one sentence per stage:

- A GPU **voxel window** (R8Uint, toroidal, `draw_dist` chunks on a side) plus a coarse occupancy texture, both fed by
  byte- and clock-budgeted CPU classification.
- Two **toroidal ambient-cube probe cascades** (C0 fine and near, C1 coarser and wider), updated by a compute kernel
  over a permuted round-robin: one workgroup per probe, a fixed ray set folded to 6 axes, blended with a hysteresis
  ladder. Rays terminate in last frame's field, which is where multi-bounce comes from, and take one **next-event
  sample** against the emitter list per ray.
- A **half-res resolve** per frame: TWO per-pixel rays. One **final gather** — a cosine-weighted hemisphere ray about
  the exact normal, marched through the voxel window and terminated in the probe cache at whatever it hits, so a single
  ray buys the whole multi-bounce series. And one **sun ray** along the exact, unquantized caster jittered over the
  sun's own angular radius. The result **accumulates temporally** through a reprojection of last frame's buffers, alpha
  = 1/n with n climbing to `N_MAX`. Beyond the window's reach there is nothing to march, so those pixels keep the plain
  cascade gather.
- Solids bilaterally upsample that against exact face normals rasterized by the depth prepass. Water and players gather
  the probe field per fragment through gi_common.wgsl; spray and the viewmodel read a small camera-light buffer a
  1-invocation compute writes.
- One **participating medium** (medium_common.wgsl) replaces every fog model, in-scattering the same `sky_base` the sky
  pass draws.

Capture envs: `DIG_VAULT` (torch room — the NEE test), `DIG_ROOF` (sun under an open roof), `DIG_DIVE` (flooded tank),
`DIG_SHAPES` (slab/stair tops must be lit).

## What is NOT built

Named here because this document previously described several of these as done:

- **No neighbourhood clamp.** The a-trous filter exists (`gi_atrous.wgsl`, two iterations), but the history is validated
  and travel-capped rather than clipped to the current frame's local range. A clamp needs the current frame's
  neighbours, which means splitting the resolve into a raw trace and an accumulate pass; at 1 spp its value is also
  doubtful, since a 3x3 of one-ray samples has a sigma wide enough to admit almost anything. SVGF itself does not use
  one. Disocclusion is rejected and then, where indirect is the light, PRIMED from the probe cache rather than filtered
  (R15).
- **No tier scaling, and that is now a DECISION rather than an omission.** Every platform gets desktop budgets and an
  uncapped half-res resolve. The web row of the original design was never implemented, and web keeps desktop settings
  for the time being. What is known: at a matched window size with the world quiesced, the browser is the slower of the
  two — but by how much is NOT established, because Chrome caps requestAnimationFrame on an unfocused window (measured
  with `hasFocus` false) and every web reading drifted downward over the first minute as focus settled, with CPU `busy`
  flat while `out` grew. Any ratio quoted from those runs is measuring throttling as much as cost. M5's cadence cut
  moved frame time clearly, and it applies to both platforms equally, so whatever the gap is, it narrowed. Whoever picks
  it up: re-measure with the Chrome window genuinely focused for the whole run, and note that capping the half-res
  target is not a one-line change — chunk.wgsl's bilateral upsample hard-codes the ratio
  (`base = (vec2i(i.pos.xy) - 1)
  / 2`), so the real gi/full ratio has to be plumbed in first.
- **The atlases are still ping-ponged 3D textures** with a full GPU->GPU copy per frame. That copy was M5a's headline
  target and it was measured out: it costs a sliver of the frame far too small to justify anything. Pricing it by ADDING
  four more copies is the exact measurement, because the result stays bit-identical; pricing it by DELETING it says the
  opposite (the atlas then holds garbage, which changes the data and the downstream workload with it — that build
  measured *slower*). The storage-buffer rewrite would touch five files and put storage buffers in fragment-stage bind
  groups on WebGPU, which is real portability risk for a cost that does not register. The cleanliness argument — nothing
  ever sampled these with hardware filtering, so the texture buys only the copy it forces — still stands on its own.
  `gather_prev` DOES now bound itself to the cascade's coverage and validate the toroidal checksum, matching the
  fragment side (M5b).
- **The voxel window still covers half the streamed radius, and that is now a DECISION.** It is built `draw_dist` chunks
  on a SIDE while terrain streams to `draw_dist` chunks of RADIUS — half the reach — so 9.2% of pixels have a sun ray
  that leaves the window and is answered "fully lit" on no data, along a boundary centred on the camera. That statistic
  is real and it is very nearly INVISIBLE, which is the part that took measuring. Building a window that genuinely
  covers the streamed radius (doubling the side AND lifting `engine.rs`'s upload clamp, which is what the earlier
  attempt missed) and holding the sun ray's reach as the only variable moves the image by mean 0.0169 — at the 0.007
  repeat-noise floor. Distant terrain is mostly lit anyway, so "lit" is usually the right answer, and by the window's
  own half-reach the participating medium has faded the surface most of the way into the sky. What DID matter at that
  range was the final gather, and it cost nothing to fix — see `gather_max` in gi_resolve.wgsl. With the gather fixed,
  the whole remaining benefit of a full-coverage window is mean 0.0556, confined to the horizon band, bought for a
  QUADRUPLED window texture — **+75 MiB** (note `gpu_buffer_bytes` in the metrics counts mesh buffers only and does not
  see it) — and an initial fill slower in the same proportion, since the upload loop is byte-budgeted per frame.
  Declined on that trade, and those two figures are the trade. The earlier "doubling helps a lot and costs no frame
  time" note was **retracted**. That build could not have been measuring new data: `engine.rs` clamps the upload window
  to `draw_dist` independently of texture size, so the outer half of the texture was never written, and wgpu
  zero-initializes it — which `gi_cell` reads as air, the same answer the narrow window gave by breaking at its bound.
  Confirmed directly: with the debug view's `cam.w`-scaled depth cue neutralized so a diff means geometry alone,
  `DIG_GI=window` at the shipped side vs a doubled one differs by a literal handful of pixels in a multi-megapixel
  frame. The apparent win was several `side`-derived bounds moving at once against unchanged data, and most of it was
  `gather_max`. Whoever revisits this: the honest fix is not a bigger box. The window's Y extent is the full world
  height and most of it is empty air, so bounding Y is where the memory actually is; and the far field plausibly needs
  only coarse occupancy — a small fraction of a full-identity window's bytes even at the doubled side — rather than full
  material identity, since the gather ray only reaches `GI_GATHER_T`.
- **No probe classification**, and **the axis fold is still lopsided** (`update_probes` barriers the workgroup and then
  leaves one thread per ambient-cube axis alive, each running a serial RAYS-iteration reduction while the rest idle).
  Both were M5b items and both were measured out, because M5a removed their justification: after the cadence cut the
  probe pass is a small enough slice of the frame that halving it cannot pay for itself, measured with an interleaved
  probes-per-dispatch bracket. Classification might halve that slice in a static scene and tree-reducing the fold takes
  a piece of it — a fraction of a percent of frame each, for real complexity and, for classification, real staleness
  risk. Revisit only if the probe pass grows again.
- **No compute port.** The resolve is still a fragment pass. Its stated motivation was avoiding derivatives, which the
  rasterized normals removed, and tile classification, which nothing uses yet.
- **The accumulator lives in SCREEN space, and that is a decision — the world-space alternative was considered and
  rejected.** Most published path-traced GI keeps its radiance cache on surfaces rather than on screen (Lumen's Surface
  Cache, NVIDIA SHaRC, EA SEED's GIBS/surfels), and the usual objection to doing it here does not apply: the surface
  parameterization problem that costs Lumen mesh cards and SHaRC a spatial hash is FREE in a voxel world, where a face
  is exactly `(cell, axis, sign)` and adjacent same-axis faces are contiguous, so bilinear works with no lightmap seams.
  The engine already has a toroidal GPU voxel window with that addressing. It would buy two things the screen
  accumulator cannot: convergence that SURVIVES motion (no reprojection at all, so nothing to smear and no travel cap),
  and a large cost saving, since every pixel currently traces a sun DDA every frame where SHaRC's update pass traces a
  small fraction of them. It was rejected on **resolution**, which is a hard arithmetic wall rather than a matter of
  effort, and the arithmetic is the argument: the sun ray's penumbra 3 blocks from its occluder is ~0.05 blocks wide,
  while an affordable dense cache is ~4×4 texels per block face (±32 blocks, 3 faces/cell, 2 B = 25 MB), which quantizes
  that nearly-sharp edge into quarter-block stairs exactly where a player is looking. 16×16 resolves it and reaches only
  ±16 blocks inside 50 MB. There is no version of those numbers that both resolves the penumbra and covers a room.
  SHaRC's answer is a camera-distance-dependent cell size, which reintroduces re-bucketing at every level crossing — a
  per-frame reprojection problem in world space, which is the problem the whole exercise was meant to remove. Three
  further unsolved pieces sit behind that one: eviction as the window slides, sun-motion invalidation, and shading faces
  the camera has never seen. What made this a QUALITY question rather than a bug fix is that R9 already closed the
  defect that motivated it — walk residual 0.922 → 0.578, flyover 2.84 → 2.08, for 0.14 ms — using SVGF's spatial half
  instead. Indirect should stay on the probes regardless — it is smooth and does not smear — so the cache would only
  ever have carried sun visibility. Reopen only if "converged while walking" becomes a requirement;
  denoised-while-walking is what exists today.

## Measuring motion

Every other capture in this engine is a still, which is the one case where a temporal artifact has already converged
away — so for a while the system had no way to photograph its own worst failure mode. `--fly N` flies the bench path N
frames and then FREEZES the camera; `--warp N` lands on that same pose with no motion behind it. Pair either with
`--frames` to choose how long the camera then sits still. The pose is identical by construction: `--warp` runs the
identical step loop, because the yaw accumulates through N `look` calls and a closed-form `yaw * N` differs in the last
ulp — a subpixel shift across the whole frame, which would swamp the diff being read. The flight is a pure function of
the frame index, so a repeat run reproduces essentially exactly — far tighter than the interactive capture-noise floor,
which is what makes small differences readable at all.

The A/B that matters is `--fly N --frames M` against `--warp N --frames M`: same pose, same duration, same everything
except whether the first N frames moved. On a correct temporal system that difference decays to the noise floor.

**With one caveat, learned the hard way in R13: the warp half is not a fixed reference.** Any change to the accumulator
can move it too, and a change that shortens a STILL camera's history blurs the reference toward the thing being measured
— which reads as a large improvement and is none. When the change under test touches accumulation, pin a converged frame
once (`--warp N --frames 3000`) and measure every variant against that. Low-pass the diff 8x8 while you are at it: a
short history and a smeared one both differ from a converged frame, and only the smear survives a box filter.

**`--walk` is the second regime, and it is the one players report from.** The bench path is a fast flyover well above
the ground; a screen-space temporal artifact scales with how fast the IMAGE moves, so that path photographs one end of
the range and a walk photographs the other — eye height, walking pace, ground a few blocks ahead, where one half-res
texel covers far more world per pixel. `--walk` modifies either `--fly` or `--warp` to swap the flyover for an
oscillation about the spawn point, one block forward and one back at eye height on a fixed period. It oscillates rather
than travelling so the column under the feet stays fixed and the eye cannot step a whole block mid-path, invalidating
history for a reason that is not the one under test. Repeat noise on both paths is negligible.

This matters more than a second data point: the smear R9 fixed measured several times larger on the flyover than on the
walk, and it was the WALK crop that showed it as a picture — the flyover's larger number is mostly disocclusion, which
is not a bug. Read the two together, and crop before believing a global mean: R9's artifact sat on one shadow and the
flat foreground diluted it severalfold.

**The first thing it caught: the accumulator froze.** The resolve blends with `alpha = 1/n` up to `N_MAX`, and the
history was rgba16**float**, whose ulp near a stored value of 1.0 is far larger than one such update, so
`hi + (indirect
− hi)/N_MAX` rounded to ZERO unless the difference was already large. A wide band of the signal range
never converged: each pixel froze at whatever partial value it held when the camera stopped, leaving a stale pattern
locked to the path walked, permanently. That was "weirdness that moves with you".

Measured before the fix: walked-vs-warped at the same pose stayed high after tens of seconds of stillness, against a
floor several times lower with accumulation switched off entirely. Everything upstream was exonerated by measurement,
not by argument — the voxel window is bit-identical after a walk, the C0 probe field converges to a unique fixed point,
and disabling the sun cone jitter changed nothing. The N_MAX sweep grew and then *saturated* rather than continuing to
improve, which is a quantization freeze and not slow convergence, and the residual was worst in the brightest part of
the frame exactly as float-ulp scaling predicts.

The fix is `gi::GI_ACC_FORMAT` = rgba32float: walked-vs-warped dropped to the same floor as no accumulation at all,
frame-to-frame stability at rest improved by nearly an order of magnitude, and there was no measurable frame cost at all
— the resolve is DDA-bound, not bandwidth-bound. It costs more VRAM for the half-res targets, which is the web tier's
problem, not the desktop's.

This was the same failure the rgba8 target before it had ("in 8 bits that rounds to zero and the average stops moving"),
recurring one format up. The rule that generalizes is not "use a float format" but that the mantissa must carry the
smallest increment the history length asks for; `history_format_resolves_the_accumulator` in gi.rs pins N_MAX against
the format so raising one without the other fails the build rather than the picture.

**What is left is the probe cache's own transient, and it is not a restart bug.** With the freeze fixed,
walked-vs-warped decays properly, falling monotonically with settle length from a large residual after a couple of
frames to the no-accumulation floor after a few thousand. Two plausible causes were tested and both were measured out.
Seeding a remapped probe from its converged neighbour toward the camera, instead of restarting it at alpha 1.0 on one
dispatch's estimate, changed **nothing** at any settle length; injecting a deliberately absurd value down that same path
barely moved the screen at all, which is the real lesson — an age-0 probe is trilinearly averaged over 8 probes,
bilaterally upsampled and then temporally accumulated, so it barely reaches the screen. The premise was wrong too: a
texel is remapped only every few seconds of travel and is swept many times in between, so the field is not "permanently
mid-restart" while you walk. And raising `PROBES_PER_DISPATCH` far enough to sweep every probe every frame — no probe
ever stale — bought only a modest fraction of the residual at the shortest settle.

So the transient is inherent to a world-space cache at this cadence, and the answer was not to sweep it harder but to
stop making the near field depend on it. **That is what the final gather did**, and the harness scored it:
walked-vs-warped fell at every settle length, and by the largest margin in the middle of the decay, while the cost went
DOWN, because the 8-probe blend with a relocation walk and an occlusion march at every primary pixel went away with it.
One cosine ray plus a nearest-probe cache lookup is cheaper than the gather it replaced.

Being a 1-spp stochastic estimate, it costs a little at-rest noise — a rise that stays a small fraction of an 8-bit LSB.

**Then a player reported a smear, and it was the a-trous pass being missing.** Walking forward dragged the shadow behind
a block forward; walking back the lit ground ate it away. `--walk` reproduced it, and cropped to the pit a feathered
grey trail pulled several blocks out of the shadow. The bisect was decisive — with accumulation switched off the walked
and warped frames agree to **essentially nothing**, so the voxel window and both probe cascades are bit-identical either
way and every bit of it lived in the screen accumulator. At a very short `N_MAX` the smear is simply gone and the crop
matches the still frame; at the shipped one it is severe.

The mechanism is resampling, not reprojection: reprojection is exact for static geometry, but the fetch still lands
between texels, and carrying a running average through hundreds of those drags the image along the direction of travel.
Three things were measured, in order:

- **A nearest history tap was quantizing the fetch** — replacing it with a bilinear fetch validated per tap cut the
  residual clearly on both the walk and the flyover. Note it changes the resting image noticeably as well — concentrated
  at depth discontinuities, where a half-res reconstruction is ambiguous whichever tap it picks.
- **An adaptive cap keyed on the history's own 2x2 gradient did exactly nothing** — the residual and its max did not
  move by a single count. It read the gradient from the ALREADY-SMEARED history, so the smear flattens the edge that
  should have triggered it. Removed rather than tuned; a real neighbourhood clamp needs the current frame's neighbours,
  which a fragment pass has not got.
- **The cap belongs on reprojection travel**, which is the honest statement of when reprojection is lossless. Two things
  taken as settled here were not, and R13 below corrects both: travel is NOT zero at rest, and a smooth
  `N_MAX/(1 +
  TRAVEL_K*travel)` is a per-frame RATE where the blur it bounds is not.

None of that is affordable without a spatial filter, because a short history is a noisy one: `gi_atrous.wgsl` is SVGF's
other half, two edge-stopping iterations with the second at twice the first's tap stride. It needs **no moments
buffer**, which SVGF spends a third ping-ponged target on, because both estimators here have an analytic variance from
data already stored — sun visibility is a mean of n Bernoulli trials, so its standard error is exactly `sqrt(p(1-p)/n)`.

Final: both the walk and flyover residuals fell substantially, well clear of the repeat-noise floor, and the crop is
clean. Cost, on a thermally settled machine, was a fraction of a millisecond. The 5x5 B3 kernel SVGF specifies was built
and measured too — slightly better quality for several times the cost — and is the one to reach for if the history floor
ever drops below `N_MOVING`. The filter barely changes a converged frame and skips entirely above `N_CONVERGED`, where
it measures as a no-op.

### R13 — the cap read the wrong variable, and rest was not rest

R9's bisect ran entirely on `--walk`, whose ground sits a few blocks ahead. That is the only part of the frame where
per-frame parallax is large, so it is the only part where the travel cap ever engaged; everything past the near ground
kept walking around near `N_MAX`. A player reported it as a shadow some way out that slides forward with them and snaps
back on approach, which is the exact shape of "the fix works in the near field only". Approaching does not repair
anything — the blur is roughly constant in screen texels, and walking closer magnifies the feature past it.

Measured on `--walk`, history depth by distance band against the walked-vs-warped residual: the near ground ran a short
history and a small residual, the mid band a deeper history and a larger one, and everything from a few blocks out to
the horizon sat near `N_MAX` with by far the largest residual — every band well above its own repeat noise. n and the
residual track each other monotonically, and the far field is doubly exposed: `gi_atrous` skips any pixel at or above
`N_CONVERGED`, so a substantial slice of the band that most needed the spatial filter was the band not getting it.
Visualizing n directly — the resolve already stores it in `sun.a` — shows it as a red cap over the horizon and
near-black under the feet.

**Rest is not rest.** The fetch round-trips a world position out through `inv_view_proj` and back through
`prev_view_proj`, and against a far plane hundreds of blocks out f32 does not return what it was given: a dead-still
camera measures a **fraction of a texel** of drift, near and far alike. That costs twice. The cap reads arithmetic as
movement — and the bilinear fetch mixes in neighbours every frame at a history depth near `N_MAX`, which compounds into
a multi-texel streak visible in a STILL frame. That streak was in every capture this project takes: the vault's torch
pool had two vertical bands running out of it, and the resting terrace faces were mottled rather than flat. The earlier
`TRAVEL_K` note cannot have measured what it was read as; at the drift actually present that setting caps n in the low
tens, and a still camera's image cannot have been nearly unchanged.

The fix is two lines and one idea: **snap the fetch onto the lattice when the miss is under the arithmetic floor, then
make the cap two-state.** `TRAVEL_EPS` clears the measured drift and stays well under the slowest real motion in the
frame (the far field's median on `--walk`). Snapped means bilinear degenerates to a copy of the pixel's own history, so
a standing camera is exact and keeps its full `N_MAX`; anything else falls to `N_MOVING`. There is no useful middle, and
the smooth form's mistake was pretending there was: once the fetch is off-lattice the blur compounds at a rate the
sub-texel PHASE sets rather than the speed, because a pixel creeping a hundredth of a texel a frame still sweeps the
whole phase range across a history that long. `a_moving_pixel_always_gets_the_spatial_filter` pins
`N_MOVING <
N_CONVERGED` so the relationship the whole trade rests on cannot lapse again.

Measured against a common converged reference (`--warp 600 --frames 3000`), with the diff low-passed 8x8 first — a short
history and a smeared one both differ from a converged frame and a raw mean cannot tell them apart, but Monte Carlo
noise averages away under a box filter and a displacement does not — the far field, from a few blocks out to the
horizon, moves on every axis at once: the smear against converged truth roughly halved; mean n while walking fell
sharply, which is the cap finally engaging out there at all; the à-trous skip while walking went from a substantial
slice of the band to none of it; and mean n AT REST rose to nearly `N_MAX`, which is the snap doing its half. Measured
against the convergence floor — the same reference against a still capture — the excess above that floor fell by well
over half. Cost is free: frame time, GI trace and denoise all stayed inside bracket noise. The four pinned stills all
move, and all move the same way — the streak coming out.

**A false positive on the way, recorded because it is the trap this harness sets.** The first form bounded `n * travel`,
the total slide over a history's life, with no snap. It measured a large far-field improvement in walked-vs-warped and
read as a clean win. It was not: with no snap, the resting drift capped a STILL camera to a short history as well, so
the reference had been blurred to match the thing under test. `--fly` vs `--warp` compares against a reference the
change under test can move. Anything touching the accumulator has to be measured against a FIXED converged frame, not
against its own warp.

**The standing question this leaves is where the accumulator should live at all.** Screen space is used everywhere in
the literature but SHORT: SVGF-class denoisers accumulate a few dozen frames at most and take the rest spatially.
Long-lived accumulation moved to world and surface space precisely because it does not reproject — Lumen's Surface
Cache, NVIDIA's SHaRC, EA SEED's GIBS, and HPG 2024's on-surface caches, which drop the screen/world hybrid outright on
the grounds that "no reprojection is necessary in between frames, as the data is directly stored on the surface". A
voxel world gets that nearly free, since a face IS `(cell, axis, sign)` where Lumen needs mesh cards and SHaRC needs a
spatial hash. What stops it being obvious is resolution: an affordable per-face cache is a few texels per block face,
and the sun's penumbra close to its occluder is a small fraction of a block wide, so it would quantize a nearly-sharp
edge into visible stairs exactly where a player is looking.

### R14 — the sun ray stopped being an estimate

R13's standing question dissolves once the right question is asked: the only reason sun visibility needed a LONG screen
history at all was that the penumbra was estimated with one random cone sample per pixel per frame — a 1-spp Bernoulli
trial that takes hundreds of frames to converge. Every smear fix so far tuned how that history is carried; R14 removes
the need for it. This is the production shadow-denoiser pattern (NVIDIA SIGMA, AMD FidelityFX): trace toward the sun's
CENTER deterministically, record the occluder distance, and reconstruct the penumbra SPATIALLY with a filter width of
`occluder_t x tan(cone)`. The reopen condition R13 set for the world-space cache — "converged while walking becomes a
requirement" — is met without building one, by making the signal cheap to converge instead of expensive to carry.

The pieces, each load-bearing:

- **`gi_sun_vis` returns the occluder distance** — free, it is the DDA's own `r.t` at the blocked return. The resolve
  converts it to half-res texels through the screen derivatives (already computed unconditionally for Tint), so
  projection and grazing angle are both accounted for per pixel, with no new uniform plumbing.
- **The cone offset is STATIC, and anchored to the WORLD.** The offset cannot be dropped (the caster is planar and
  rational — dropping it is the lattice resonance), but it can stop being temporal noise: hashed from a sub-block world
  lattice (the hash point pushed half a lattice step INTO the block behind the face, because a shading point sits
  exactly on the lattice plane the quantization cuts along), a world point asks the same question every frame FROM EVERY
  CAMERA. Screen-hashed was built first and shimmered: a moving camera slides every world point across the pixels,
  re-asking a different fixed question each frame — measured as a clear rise in consecutive-frame flicker across the mid
  terrain bands on the flyover, and mouse-look is exactly that motion at full screen speed. The world anchor is
  world-space accumulation's stability property without its storage.
- **Sun history is capped at `N_SUN` frames, still or moving** — enough to antialias the dither, short enough that smear
  is bounded BY CONSTRUCTION (an N-frame average can only be dragged N frames). Swept a rung either side on the shadow
  band: shorter shows the dither through, longer re-admits drag; the shipped value stands. Indirect keeps its own cap.
- **`n` moved into the sun target's g channel**, packed above the face code (exact in f32's mantissa, pinned by
  `packed_metadata_stays_exact`) — WebGPU's baseline attachment budget is spent, so the penumbra width earned sun.a by
  the pack. Every reader is a `textureLoad` that masks the code back out.
- **`gi_atrous` gates per signal now.** Indirect keeps the original SVGF trade (filter below `N_CONVERGED`, skip above).
  The sun's filter is not denoising but RECONSTRUCTION, so it runs at rest too — a deterministic ray never builds a
  penumbra in time, and the old skip would have shipped stair-edged shadows on every still frame. Inside the penumbra
  regime there is deliberately NO luminance edge-stop (the 0/1 boundary is the thing to blur; pen -> 0 collapses the
  kernel to identity, so contact stays hard), the width itself spreads (max over accepted taps, so the LIT side of an
  edge blurs by iteration 2), and the sun taps' stride scales up for wide penumbras (`the_penumbra_fits_the_filter` pins
  PEN_MAX to the combined support). One shared tap ring serves indirect, Bernoulli sun and the spread — splitting them
  into three loops was built first and measured clearly slower; sharing the loads brought it back.
- **`N_MOVING` roughly doubled, and R14 is what made that legal.** Pre-R14 the sun smear scaled with it and pinned it;
  with the sun on its own cap, deeper indirect history buys flicker without smear. The 1-spp gather's moving flicker is
  very visible in a torch-lit interior (DIG_VAULT on the walk path, consecutive-frame diff): it falls with every rung of
  deeper history — but the rung past the shipped one overshoots, and the indirect's own resampling blur comes back with
  it, the shadow band getting markedly worse while non-shadow rows barely move. The shipped value is the sweet spot on
  both axes at once. Note the vault metric is IDENTICAL pre/post R14 at equal N_MOVING — that flicker predates R14 and
  was unmasked by the smear's removal, not caused.
- **Lighting edits invalidate screen history.** The probe pass already consumed a bounded few edit spheres a frame; the
  same spheres now ride the resolve uniform for a couple of sweep periods and cap `n_hist` at `N_MOVING` inside them — a
  placed torch's pool lands in a fraction of a second instead of fading in over hundreds of frames on surfaces whose
  face code and depth never change. Continuous time-of-day drift gets the same property free from `N_SUN`; the
  `SUN_JUMP_COS` full reset stays for `/time`.

Measured on `--walk` against the fixed converged reference, in the shadow band (the rows where the between-build delta
lives — everything outside them is bit-identical across builds, which is itself the cleanest proof the sun change
touched only the sun): the residual fell in two clear steps, first from the deterministic sun ray with a screen-hashed
cone, then again — by more — once the cone was world-anchored and `N_MOVING` raised. Measured as excess over the
non-shadow floor, which is the honest form of the number, almost all of the gap closed.

At-rest frame-to-frame stability halved — the resting dither no longer flickers at all, it is a constant. The four
capture envs moved only in penumbra texture; the vault (sun-free) is byte-for-byte the same lighting.

**Measurement traps this round added to the list:** a seedless capture rolls a NEW WORLD (two "repeat" runs once
compared two different planets); the walk pair needs `--tod` frozen or the 3000-frame reference sits under a different
sun than the walked run and the diff measures the sky; `--frames` is the TOTAL frame the screenshot fires at
(`--fly
600` wants `--frames 601`, not 1); `--tod` is dev-profile-only; and consecutive `just bench` runs drift
thermally by more than the effects under test — only interleaved brackets are comparable, and the flyover's
consecutive-frame diff cannot see rotation-only artifacts because its translation term dominates.

### R15 — a reveal starts from the cache, not from noise

The reported artifact: in indirectly-lit places — in shadow under a cliff, in a cave — surfaces the camera reveals
(looking down a cave mouth, strafing or backpedalling so new world enters at the screen edge) "ghost" for a split second
before settling. The mechanism is the disocclusion path: a pixel with no valid history restarted at n = 1 from its own
raw 1-spp gather, and where indirect light is the WHOLE signal that single ray is speckle — a cave floor's ray mostly
finds dark walls and occasionally the bright mouth — so the revealed region visibly developed while n climbed through
N_MOVING. Sun-lit reveals never showed it because the deterministic sun ray is converged from its first frame.

The fix is the one the literature converges on (AMD GI-1.0's hash-grid fallback, Lumen's world-probe fallback at failed
reprojections, NVIDIA's irradiance-cache disocclusion mix; ReBLUR's "history fix" pass is the same idea built by someone
with no world cache to read): answer the no-history pixel from the world-space cache. This engine already owns one — the
probe cascades, converged and resident — so a disoccluded pixel now primes its accumulator with the cascade blend at
`N_PRIOR` virtual samples and lets the ordinary 1/n machinery decay the prime out. No new buffers, no per-frame blend:
the prime is an initial condition, and its residual weight after k real samples is `N_PRIOR/(N_PRIOR + k)`.

Two gates, both measured in rather than assumed, because the first cut fired on every miss and was a large regression:

- **A grazing miss is not a reveal.** The near ground fails all four history taps by a hair on most walking frames
  (adjacent half-res texels on a grazing plane legitimately differ by more than the flat depth tolerance), and the
  walk's backward half reveals the whole bottom screen edge every cycle. Primed indiscriminately, walked-vs-converged
  blew up by most of an order of magnitude — worst in the bottom third of the screen, and linear in `N_PRIOR` — the near
  field locked at cache lighting, which is exactly the contact darkening the cache cannot see. So a same-face tap that
  missed depth by ≤ `PRIME_MARGIN` tolerances testifies the surface never left the screen, and the pixel keeps the old
  unbiased n = 1 restart. A true reveal's tap held the OCCLUDER, whole blocks nearer — different in kind, not degree.
- **Only where indirect is the light.** A distance ramp was tried first (no prime near the camera, full strength some
  way out) and just moved the bias to whatever band the ramp allowed — the flyover's mid-distance ground came back
  several times worse than its baseline. The partition that works is the sun: where `sun_vis x N·L x above_horizon`
  clears `PRIME_SUN_MAX` the gather is a minor correction and the prime only pollutes it; where it doesn't — shadow,
  cave, night, exactly where the ghost lives — the prime runs at full strength.

Gated, the sun-lit regimes return to baseline (the walk is unchanged within noise, and the flyover's consecutive-frame
flicker actually improves, because cache values are temporally stable where speckle is not), and the target case moves:
DIG_VAULT under `--walk`, the torch-lit interior where every reveal is indirect, drops its walk flicker clearly — the
bottom third halves — and its walked-vs-converged falls too, most of all in the wall band. The flyover's
walked-vs-converged rises in its shadowed distant bands and that is the intended trade read by a metric that counts bias
but cannot see variance: those bands now show smooth cache lighting for the `N_PRIOR` frames a reveal used to spend as
speckle. The vault's converged still is unchanged.

Two follow-ups from the first play test, same day:

- **The prior is `gi_cache_at`, not `gi_indirect`.** Priming is bursty by nature — a camera swing into a cave mouth
  reveals a hundred thousand half-res pixels in ONE frame — and the full cascade blend is many dependent loads per call
  (8 probes × relocation walk × occlusion DDA, twice), a call class already measured to cost a large fraction of the
  frame rate when it ran per-pixel. That burst was a visible frame spike on every cave look-in. The nearest-probe lookup
  that already terminates every gather path is plenty for a prior that decays out in `N_PRIOR` frames.
- **The history depth tolerance widens to the surface's own gradient — but only on a total strict miss** (SVGF's
  `fwidth(z)` rule, the one this system had skipped): a flat tolerance rejected every tap on every frame for any surface
  seen nearly edge-on — an east face viewed looking north — because adjacent half-res texels on a grazing plane
  legitimately span more depth than that. Those surfaces churned at n = 1 as a distracting flicker band precisely where
  a face turns away. The tap loop now runs strict-first: pass 0 is the old flat tolerance byte for byte, and only when
  it comes up EMPTY with a same-face near-miss on record does a second pass rerun with the tolerance widened to
  `GRAZE_K` texels of the pixel's own per-texel depth span (`texel_world`, already live for the penumbra), capped at
  `PRIME_MARGIN` base tolerances — sharing the reveal threshold, so a silhouette gap stays rejected and the graze/reveal
  classification cannot disagree with the acceptance rule. An always-widened single pass was built first and measured
  clearly slower on the bench for the whole screen's benefit of nobody; the two-pass form confines every new instruction
  to the churn population. Measured (always-widened variant; the two-pass accepts a superset of nothing it accepted):
  the walk's smear guard IMPROVED slightly — the walk translates toward/away, so its taps already passed; rotation past
  an edge-on face is the case no harness path exercises, which is exactly why the artifact survived this long — vault
  moving flicker improved, and the vault's walked-vs-converged gave back R15's earlier gain to sit at parity with
  pre-R15, the deliberate trade of churn for a mild motion-blur that recovers at rest, plus a coarser prior.

### R16 — a slab shadows as a slab

Shaped cells classified OPAQUE in the voxel window ("the same v1 approximation fluid makes"), so a 1/3 slab cast a
full-block shadow onto the slab beside it, a torch post shaded like a pillar, and an open door blocked its own doorway.
The literature check turned up the same artifact and the same fix shipped in SEUS PTGI ("blocks which are not a
full-sized cube... aren't considered properly in lighting" -> per-shape sub-box tests behind a quality toggle) and
formalized as ESVO's per-voxel "contours" (Laine & Karras 2010): keep the grid, and clip the ray's in-cell [t_entry,
t_exit] span against sub-cell geometry stored per class. The alternatives — fractional opacity per cell (VXGI-style,
position-blind: a grazing ray that clears the slab's top still attenuates) and finer voxelization (Teardown, memory x
resolution³) — lose to it on exactly the deterministic sun ray this engine leans on.

Classification is state-aware now: shaped cells map per (id, state) to synthetic classes from `SHAPE_CLASS_BASE` up, one
per shaped variant in use, each carrying a two-box cover of the shape's `shapes.rs` collision boxes in the widened LUT
(box 0 = collision box 0 exactly, box 1 = the AABB union of the rest; exact for slabs and torches, a stair's L rounds up
only its top rear corner; over-blocks only, never under, worst case the old full cell). A state with no collision — an
open door — classifies as AIR, which is what lets light through a doorway. `gi_boxes_hit` lives in gi_dda.wgsl so all
three marches (the resolve's sun ray, the gather, the probe kernel's trace) share one definition and a slab shadows the
cache exactly as it shadows the per-pixel ray; `gi_solid`/`solid_cell` (probe placement, the short probe-visibility DDA)
deliberately still treat partial cells as whole — the old conservative behavior, one fewer thing to re-verify.

One rule was earned by a broken capture rather than designed: **an emitting partial cell is a hit whole for the gather
marches.** The first cut let gather rays sail through the torch's cell unless they clipped the thin post boxes, and the
vault's pool collapsed — a huge old-vs-new diff — because `LIGHT_GAIN` is calibrated so the direct-hit term
`alb × emit
× LIGHT_GAIN` matches the NEE pool, and the voxel IS the emission model. With the rule, the vault's
converged still is back at the repeat floor. Only the sun ray resolves an emitter's boxes: a torch must not cast a
pillar shadow, but it must still glow.

DIG_SHAPES photographs the payoff: the showcase's row of identical full dark squares resolves into a stepped stair
profile, thin slab bands, and nothing at all under the open doors. Terrain captures move by noise only — worldgen places
no shaped cells, so the box test's cost is confined to player-built cells.

### R17 — water stopped paying per pixel for a signal that varies per probe

Standing at a river's edge looking down halved the frame rate, with the loss swinging as the view moved. The cause was
priced into the code from M2 and documented next to `gi_cache_at` without anyone adding the two costs together: water
shades its indirect term by calling `gi_indirect` — the full 8-probe cascade blend, a great many dependent texture loads
with a relocation walk and an occlusion DDA per probe — **per fragment, at full resolution**. The resolve survives that
blend only because it runs at half res with temporal history behind it; the comment beside `gi_cache_at` even records
the measurement (running the full blend per half-res pixel cost a large fraction of the frame rate). And the cost is
view-dependent by construction: `gi_cascade_gather` early-outs to the analytic dome outside cascade coverage, so an
ocean on the horizon was nearly free while a river underfoot — every fragment inside C0, beside bank geometry that runs
the relocation walks to their full length — was the worst case for the whole screen at once. That coverage cliff is
exactly the "spikes when I look down" report.

The fix is where the signal's own frequency says it should be: **the gather moved to the vertex shader**. Water quads
are strictly per-cell (`emit_water_face` — corner drop codes are per-cell state, so nothing merges), which puts vertices
at most a block apart against a probe field spaced wider than that and smooth by construction; perspective interpolation
across a quad cannot lose anything the blend could represent. Millions of per-fragment blends become thousands of
per-vertex ones, and the coverage early-outs keep distant vertices at pure ALU. The per-fragment sun DDA (`SUN_STEPS`)
stays per pixel — shadow edges on the surface are the one lighting signal sharper than the lattice, and it was never the
cost.

Two structural notes. The depth prepass gets its own position-only `vs_depth` entry so it doesn't pay the gather for
geometry it only depth-tests; both entries build their `@invariant` position from the one shared `water_world` decode,
which is what the color pass's LessEqual-against-prepass depth test demands (a divergence here is water full of
view-dependent holes — the failure mode is unmissable, and the sea capture would have screamed). And the gather bind
group (`gather_bgl`) gains VERTEX visibility — WebGPU baseline allows read-only storage and sampled textures in the
vertex stage, so this fits `Limits::default()` unchanged.

Parity: the sea-view still (seed 7, tod 45, spawn pose — open water right of frame) moved barely more than its own
same-build repeat floor (animated ripples dominate both numbers); the bottom-third band, all terrain, was unchanged.
Players (`players.wgsl`) keep the per-fragment gather deliberately: a player model is a few thousand fragments, not half
the screen.

**The honest postscript (R18's measurement):** this fix was real but it was only a slice of the water pass, not the
whole story — the water pass got clearly cheaper at a small window, and the user's play test still correctly reported
"no improvement." What the first diagnosis missed is in R18.

### R18 — the sun march learned where the sky starts

The user's report survived R17, so the guessing stopped and the scene got a harness pose: `DIG_RIVER=1` excavates a wide
flooded basin, several blocks deep, ahead of the spawn camera (pair with `--pitch -35`), because NO existing capture put
a shaded water surface on screen — DIVE is submerged, the spawn sea is far out, and that gap is exactly where R17's
"fixed it" came from. Bisecting on that rig at the user's window size (a Retina window; `--size` holds the logical size
but the physical size follows the display — verify the capture's pixel dimensions before comparing runs, two of these
runs silently landed on different backing scales):

- Stubbing the SSR march barely moved the frame. Not the cost — looking down, the reflected ray exits the screen in a
  few steps.
- Stubbing the water sun DDA removed the ENTIRE river-over-terrain premium. Every iteration of its capped loop was spent
  proving "open sky" — the loop had no early exit short of the top of the world, so every water fragment on a screen
  full of them paid a string of dependent texture loads to learn nothing was there.

Two fixes were built; ONE survived measurement. The super-brick skip (`gi_ray_skip_box`, a coarse mip above the brick
level) lost both ways it was wired: checked before the brick it taxed every dense iteration of a latency-bound loop (the
river pose got clearly slower), nested behind the brick it taxed every empty one, and at terrain altitudes a cube that
large almost always holds SOMETHING — the world's occupancy lives in a thin shell around sea level, so a cubic skip
level asks the wrong-shaped question. Deleted, not shipped. What shipped is the **sky ceiling**: the window's highest
occupied cell y, scanned on the CPU from the coarse-occupancy mirror whenever a chunk classifies (`sky_ceiling` in
gi.rs), carried in `frame.gi_misc.y` / `rz.jitter.z` / probe `zenith.w`, and tested per iteration in every march as one
float compare: a non-descending ray above it is a proven miss/lit. The climb now ends at the tallest local hill instead
of the top of the world.

Measured at matched windows, settled intervals: the river pose and the terrain-only control each got faster by about the
same absolute amount — the ceiling pays everywhere (every resolve sun ray and probe ray climbs), not just on water.
Parity: dive (underwater sun transmittance) sits at the repeat floor; the sea-view terrain band is unchanged; the sea's
water band moved only inside the distant glint stripes, which is ripple PHASE — the animation clock reaches a given
frame at different wall times on different builds, a capture-harness trap to remember alongside the seedless one. A
river-over-terrain premium remains, and it is still substantial: the sun DDA's surviving iterations near banks plus
water's ordinary per-fragment work (ripples, refraction, medium). The next lever if it's ever needed is a per-column
max-height texture (a real heightfield DDA answers "how far up until sky" in one load where the cubic mip could not); it
was scoped and deliberately not built — diminishing returns until a player reports it.

### R19 — the column field: built, measured, declined

R18's scoped next lever got its trial and did not ship; the full patch is parked at the `archive/r19-column-skip` tag.
What was built matched the scoping exactly — a per-column max over every NONZERO class (water included, since the field
licenses SKIPPING and the coarse byte's any-solid rule would let a jump lose the pool `sv.y` counts), merged per chunk
into a side×side R8Uint field, and a `gi_ray_skip_column` that jumps a dense column bit-identically to stepping it.
Measurement then cut it to one call site: with the gate in BOTH gi_common marches the resolve's trace cost went UP — the
river pose measurably slower — because an underwater pixel's sun ray sits below the water-inclusive colmax for its
entire climb and pays the dependent 2D load per dense iteration without ever skipping, the super-brick lesson in a new
hat. The one ray that profits is water.wgsl's `SUN_STEPS`-capped sun march, launched at its own column's waterline; even
there the gain was correctness, not milliseconds (frame cost inside bracket noise on river/shore/terrain): that capped
budget TRUNCATES into "lit" against far banks (`gi_sun_vis` answers lit when it runs out, which shows up in the water
bands against a build whose cap is lifted far enough that it cannot truncate), and the skip let the same budget reach
the real occluder. That look change was declined at sign-off, so the truncation stands as a KNOWN, accepted artifact:
distant shorelines render slightly un-shadowed. If a player ever reports it, start from the parked branch, not from
scratch.

Two harness traps earned their entry this round: **physics ticks ride the wall clock**, so two builds of different speed
reach `--frames N` with the spawn settle in different states and every cross-build still is polluted by a sub-block
camera shift — capture at ≥1200 frames so both builds are long settled (terrain parity then sits at about its own repeat
floor); and a coherent reflection streak shifted by ripple phase does NOT cancel under a low-pass, so water-band diffs
across builds need eyes on crops, not thresholds.

### R20 — torch light became a shadow ray, and the support block lit up

Two symptoms, one estimator. The user's "massive flicker moving near a torch" and "torches should light the block they
sit on" are the same defect seen twice: per pixel, torch light arrived ONLY when the 1-spp gather ray geometrically
struck the torch's cell (`alb * emit * LIGHT_GAIN`, on long odds even a stride away, re-rolled every frame) — a
high-magnitude Bernoulli trial history tuning was already measured unable to drown (the N_MOVING sweep at R14). And the
support block could never win even the lottery: every march steps before it tests, so a ray leaving the top face starts
INSIDE the torch cell and exits unregistered. The brightest surface in the room structurally received nothing.

The fix is the R14 move applied to local lights (Q2RTX / GI-1.0 / RTXDI lineage — direct light gets its own
deterministic low-variance estimator): the resolve binds the same `lights` buffer the probe pass NEE-samples (its second
fragment storage buffer), picks the DOMINANT in-range light per pixel (the max of `emit·ndl` over the near-clamped
inverse square distance, a deterministic pick so the pool is steady at n = 1), fires ONE `gi_march` shadow ray stopped
the NEE margin short of the light's cell, and adds the probe `nee()` formula minus the ×count pdf. The gather's emission
term for that one cell is SUPPRESSED whether or not the shadow ray connected (`path_radiance`'s `skip_cell`) —
half-suppression puts the flicker back in the torch's shadow; other emitters keep the direct-hit term since nothing
NEE-samples them. The disocclusion prime adds the (already computed) NEE term so a cave-mouth reveal beside a torch
starts at the pool every later sample votes for. `LIGHT_RANGE`/`LIGHT_GAIN` are const twins of gi_probe's, pinned by
`resolve_and_probe_agree_on_light_calibration` (which required teaching the test helper WGSL's typed const form — the
untyped pattern reported gi_probe's as undeclared).

Measured on the vault rig: the under-torch dark patch is gone; the walk's consecutive-frame flicker halved in the bright
pool band and fell clearly in the mid band — the remaining whole-frame delta is texture-edge resample shimmer under
motion, visible as the wall speckle pattern in the diff heatmap, not lighting noise. The room is legitimately brighter
overall: per-pixel direct light used to be under-counted by the hit-lottery model everywhere the probes' point-lit cache
couldn't reach except as bounce, and the ray now prices a torch exactly as the cache does — which is also what removed
the near/far seam risk. Torch shadows are hard (point light, no penumbra machinery — v1, judged at look sign-off; the
pool level moved and LIGHT_GAIN retune-in-both-files is the sanctioned knob if it reads too hot). Cost: a small trace
increase in the sealed vault (every pixel in range of the torch — the worst case; the brackets overlap same-build
spread), zero measurable in torch-free scenes (count == 0 is one header read), frame time display-paced and unmoved. The
light list also refreshes after enough camera movement when more than `MAX_LIGHTS` emitters are resident — order never
mattered to a reader, but the truncation to the nearest `MAX_LIGHTS` was frozen at whatever camera last saw a chunk
classify.

### R21 — water was marching two rays whose answers were multiplied by nothing

Third attempt at "looking into water halves the frame rate", and the first one that measured before it changed anything.
R17 moved water's indirect gather to the vertex shader; R18 shipped the sky ceiling; R19 built the column field and was
declined. The user's verdict after all three was that the cost was still there — and this time they gave the measurement
that reframed it: same spot, same draw distance, the frame rate **less than half** with a lake in view. Water was not a
few-millisecond premium, it was HALF THE FRAME.

Two things had to be built before a single line of shading changed, because both prior rounds had been reasoned from
source and measured on a pose that could not show what they moved:

- **`DIG_STUB`** (`renderer::stub`) — a uniform-branch mask that removes one shading term per run, so a single build
  attributes a whole frame. Uniform, so an un-stubbed run pays nothing and a stubbed one cannot diverge inside a wave.
- **`--pos x y z` + `--fixed-dt S`** — stand in real worldgen water rather than in `debug_river_plan`'s flat-walled box
  (a real river is TERRACED: the sim halves the level on each spread, so every step has its own level and its own
  unmergeable per-cell quad, the whole bed is bank rather than just the rim, and cascades make impact cells the box has
  none of), and step the world by a constant so two builds reach frame N in the same world state. `--fixed-dt` alone
  took the DIG_RIVER same-pose repeat floor down by orders of magnitude, to essentially zero: everything animated rides
  dt, so a cross-build capture used to measure the speed difference, in the ripple bands, far above that floor. That is
  R19's two harness traps retired rather than documented again.

The attribution, at draw 32 / seed 42, submerged looking up at the surface — the "it's slow underwater too" case that
killed every fragment-count hypothesis — ranked the terms unambiguously, every one of them far above the repeat floor:

| stub                             | what it removes                   | rank                                                       |
| -------------------------------- | --------------------------------- | ---------------------------------------------------------- |
| `wsun`                           | the per-fragment sun DDA          | the largest single term, more than all the others together |
| `wssr`                           | the screen-space reflection march | second, a substantial slice                                |
| `wvert`                          | the per-vertex `gi_indirect`      | third, smaller again                                       |
| `wripple` / `wrefract` / `spray` | ripples, refraction, spray        | at the noise floor                                         |
| `wflat`                          | the whole water fragment shader   | nearly all of water                                        |
| `nowater`                        | water entirely                    | the total, by definition                                   |

Water was very nearly half the frame, and the no-water frame ran at the rate the user reported seeing "at times" —
reproduced.

**Then the finding that mattered: both dominant terms were being multiplied by ~zero.** Water has no diffuse sun
response — a transparent surface's colour is the refracted backdrop plus the reflection, and neither uses `sh`. Sun
visibility reaches the pixel ONLY through foam (aeration, the shore line, splash cells), the specular glint and the lip
sheen. On calm open water away from the glint, and on ANY surface seen from below, every one of those weights is ~0.
Meanwhile `reflection_color` is called as a FUNCTION ARGUMENT, so its whole dependent depth march ran even where
`mirror_w` is pinned to exactly 0 (the underside of a surface, submerged). So both marches are now gated on their own
contribution: `sun_w` (the max of those five weights, times the light colour) against `SUN_MIN_L`, and the mirror march
against a mirror weight of exactly zero. This is importance, not approximation — the admitted error is the weight times
the radiance the march would have returned.

The threshold has to be in RADIANCE, and that is a lesson the parity pass taught: the first cut compared bare blend
weights, and a small Fresnel weight against an HDR sky (this engine has no tonemap, so 1.0 is merely "white") put a
ripple-banded difference across a whole basin, an order of magnitude above any repeat floor. Weighting by `light_color`
and dropping the mirror tolerance to exactly zero fixed it.

Measured after, same poses: water's own cost fell severalfold submerged looking up, taking the whole frame rate up
sharply with it; by a similar factor submerged flat; and substantially above water at a terraced river with a cascade.
Parity with `--fixed-dt`: DIG_RIVER at −35 and −60, the natural cascade pose, the submerged pose and DIG_DIVE all diff
at **essentially zero**, one pose showing a handful of isolated pixels. The two ray marches that were most of water's
cost are now a small slice of it; the biggest term left is the per-vertex `gi_indirect`, which is the measured next
lever if water is ever the problem again.

### R22 — the torch out-lit its own flame

User report: "the bright kind of overexposed looking circle right under torches", and a follow-up that the flame itself
should read brighter. Both are the same defect, and it is a DISPLAY-RANGE one — which is worth naming, because the
honest fix is the thing this engine deliberately does not have.

R20's NEE prices a torch as a point source: `emit × LIGHT_GAIN × ndl / max(d², LIGHT_NEAR²)`. The support block's top
face sits half a block from the torch's centre, so it took that term at many times a display whose white is 1.0 — a
hard-edged ellipse of saturated orange with the floor texture gone inside it. Measured in the vault band: **a
significant fraction of pixels with a channel at 255 and not one of them actually white**, which is the giveaway — this
is per-channel saturation shifting the hue to the light's, not a bright surface.

Two changes, both stopgaps for the absent tonemap and both explicitly labelled as such:

- **`LIGHT_NEAR` grew**, the pinned twin of the existing near-field clamp (which had existed only for the
  divide-by-nothing case, and is now also what keeps the near field near the white point). Outside the radius the
  falloff is exactly 1/d², so nothing past it changes at all — the floor and wall bands away from the torch barely move,
  and an outdoor scene with no emitters diffs at zero. The radius is a measured trade, not an extreme: clipping drops by
  most of an order of magnitude at the shipped value and bottoms out a rung further out, but what survives there IS the
  torch's own emissive sprite (the last rungs measure identically), and buying that last sliver costs a further chunk of
  pool brightness on a pool that already reads faint. What is left at the shipped radius is a hot core at the flame's
  base; what went away is the flat ellipse.
- **An emissive-ink gain in chunk.wgsl**, riding the ink's OWN luminance — a gain quadratic in it. A light source has to
  out-read what it casts or the picture inverts — and once the pool stopped clipping, the flame was dimmer than the
  light it threw. The weighting matters because a torch is one block but not one material: head and wooden post are both
  fullbright, and a flat multiplier lifts the post into glowing wood. This is the ink alone; `glows` and `emits` stay
  independent, so nothing here changes what a torch CASTS.

Neither touches perf (the clamp is a compile-time constant; the gain is a dot product on a fragment that already does
several texture loads). **The real fix stays on the backlog:** an HDR scene target plus one tonemap on the composite
would roll the highlight off instead of flattening it. Note the offscreen scene colour is created with the SURFACE
format today, so radiance is already clamped at the scene pass — a tonemap cannot simply be appended, it needs the HDR
target first.

### R23 — the per-block tint came back, on the albedo this time

User report: the pre-RTGI engine had a per-block tint that broke up large flat surfaces of a single block type, and the
flat RTGI look wants it back. It died at the M1 demolition (3cbed02) as collateral, not by argument: the old one
(b6af702) packed a splitmix hash of the world block coords into the top bits of the vertex word and folded it into the
AO varying, so when AO went and the vertex words were repacked tight, the tint had nowhere to live.

It does not need a vertex bit now. `fs` already carries `i.world` and the flat face code, so the block cell is
`floor(world − n/24)` — nudged half a sub-grid step along the face's own normal, because on the face's axis the world
position sits exactly on a cell boundary where `floor` answers the neighbour. That is exact for full blocks and shaped
sub-boxes alike (the nudge is finer than the thinnest sub-box), so: **no vertex-format change, no re-mesh, no mesher
touch, and no new varying** — which also means the `@invariant` contract with the depth prepass is untouched.

**It multiplies albedo, never light.** That placement is the load-bearing part, and it is a placement the pre-RTGI
engine did not have to think about: the à-trous filter and the temporal accumulator only ever see the lighting buffers,
so a per-block step written into the light is a signal they are built to smooth across exactly the edges it lives on —
blurred toward its own mean at stride 1, then averaged over the whole history. On the albedo it lands after the
bilateral upsample, where nothing downstream can filter, blur or accumulate it. Emissive ink is excluded structurally
rather than by a test: the tint is computed inside the `i.emissive == 0u` branch, so an authored flame never reaches it,
and R22's `ink`-weighted gain cannot be detuned by an albedo jitter underneath it.

Calibration was a four-way sweep over value amplitude, with hue coupled at a fixed fraction of it, captured on the
DIG_VAULT stone wall, which is the flat one-material surface the feature exists for. **The smallest rung is invisible**,
and that is worth recording because it is what the old jitter used: the mean |diff| over a daylit terrain frame is down
in the noise, and a many-patch sample of the lit ground plane moved its luma stdev by less than that. The largest rung
quilts: the wall stops reading as one material. **The middle rung ships**, straight off the sweep. The **hue ships
dialled back** from the sweep's coupled ratio on a live look — the two knobs are independent by construction (the hue
rotation barely moves luminance at all) and the sweep only ever varied them together, so the value is measured and the
hue is a look call on top of it. Note the sweep had to be shot in the dim room rather than in daylight, because an
albedo jitter scales with the light — it varies most where a surface is brightest and all but vanishes in shade, which
is correct for a material property but means a daylit capture flatters it.

Two things the sweep also settled: the hash is a lowbias32 avalanche over a per-axis odd-stride sum of WORLD coords, and
a heavily amplified diff shows crisp per-cell patches with no plane correlation along any axis and no per-chunk tiling —
chunk-local coords would tile, which is the same reason the old CPU hash took world coords. And the probes' material
albedo stays untinted, so a block's jitter does not ride its own bounce light: it is below the noise floor of a 1-spp
gather, and paying for it would mean a per-cell lookup inside every march.

### R25 — water wrote itself over the backdrop it had already absorbed

User report, with a screenshot: past some distance "the blocks under the water lose their blue colouring and look grey
instead". Reproduced exactly from the debug panel's new `pos` line
(`--seed 42 --pos 95.5 132.5 85.5 --yaw -313.7
--pitch -17.7 --tod 25.7`), which is what that line is for.

Four candidates went first, each killed by an A/B rather than by argument:

| suspect                   | test                                            | result                                               |
| ------------------------- | ----------------------------------------------- | ---------------------------------------------------- |
| `thick` wrong at range    | visualised it; CPU ray-march of the same pose   | shader and march agree — **correct**                 |
| SSR reflecting the seabed | `DIG_STUB=wssr`                                 | negligible, no hue shift                             |
| aerial haze               | `AIR_VISIBILITY` raised to effectively infinite | barely moves submerged pixels, and r/b hardly at all |
| Fresnel sky reflection    | forced `mirror_w = 0`                           | band gets dimmer, **not bluer**                      |

Then the decomposition. `backdrop_col` — the scene sample with the medium applied — has a red channel of **exactly
zero** across the whole band, which is right: that many blocks of water at `WATER_SIGMA.r` extinguishes red completely.
Yet the final pixel carried a substantial red, and `back + surf` could not account for it. The missing term was outside
the shader.

`renderer.rs` wrote a second copy of the frame UBO for the water pass differing in one field, a `misc[0]` alpha
commented "Java transparent-pass u_Alpha" — and water's alpha was built on it. That constant is from the raster port,
and it outlived the M1 demolition that replaced the absorption model justifying it. The point of the current model is
that the water fragment **samples the scene itself** and runs the transport over it: `back + surf` already IS the
backdrop, absorbed. Blending that over the same backdrop re-adds an unabsorbed copy of it, in whatever proportion the
alpha leaves.

Near shore the leak is invisible — a foot of water barely changes the bottom's colour, so the absorbed copy and the raw
one nearly agree, which is why this survived every shallow-water capture the engine has. Under deep water it was the
entire picture: the transport takes the backdrop to black, and what you see is the leaked fraction of the raw seabed in
its own grey, against nothing. Hence "blocks under the water" — they were literally the blocks, arriving by a path the
model does not have.

Alpha is now the falling-sheet streak gate alone — a clamped ramp in the streak factor `s`, which sits at its neutral
value on every still surface, so alpha is exactly 1.0 everywhere else. A torn sheet genuinely is see-through in its gaps
and `thick`, reconstructed from the depth buffer, cannot know about the tear — that one stays. With `misc.x` unread by
water, the second frame UBO and its per-frame upload went too; the water bind group binds the same buffer the solids do
(it still needs its own bind *group*, for the resize-recreated scene targets).

Measured at the reported pose: the grey band's red-to-blue ratio falls by more than half, the shallows move a little,
and the diff is confined to the water region with literally nothing outside it. DIG_RIVER and DIG_DIVE re-shot for
sheets, shore lines and the submerged path.

### R26 — the torch radiated from its handle, and the margin that hid it

Two changes, one of which only became reachable because of the other.

**The origin.** Every emitter was flattened to its cell CENTRE. For a full cube that is right; for a torch it put the
point source in the wooden post, a fraction of a block below the flame the eye reads as the source. Inside `LIGHT_NEAR`
that costs no brightness at all — irradiance is flat in there — so the only thing it moved was shadow geometry, which is
why nothing had caught it. `digblock_core::gi::light_origin(class)` now answers per class: the cell centre for anything
unshaped, and for a shaped emitter the centre of the cell's emissive band clipped to the shape's own extent. That band
is not a magic number — texgen's `torch_side` glows a fixed top fraction of its rows, and shaped UVs are cut from the
CELL's face (`uv_cell`: `v = G − 1 − y`), so where the post's fire appears, and with it the origin, is derived from the
texture recipe and the shape's own height rather than chosen. Pinned by
`a_torch_radiates_from_its_flame_not_its_handle`.

**The margin that broke.** Raising it lit four dark, cell-aligned wedges on the floor around the torch's base. NEE's
`max(dist − margin, 0)` stop-short is what stops the whole-cell emitter rule from letting a light occlude itself, and
**the old constant was the cell half-diagonal plus slack** — a sphere of that radius around a CENTRED light contains its
own cell exactly, which is the entire reason it worked. From the flame's height the cell's lower corners sit outside it,
so shadow rays arriving from below re-entered the emitting cell through those four corners and the post shadowed itself.
Attribution was measured, not argued: the wedges are present at the old gain, and `DIG_STUB=ginee` shows that whole
region going black, confirming NEE is the only thing lighting it and a blocked ray reads as a hole.

`gi_light_margin(p)` in **gi_dda.wgsl** replaces the constant with the distance from the light to the farthest corner of
its own cell, `length(max(frac, 1 − frac))` plus a sliver of slack — identically the old constant for a centred light,
wider at flame height, and correct for whatever origin a wall-mounted emitter eventually has. It lives in the shared DDA
rather than as twinned constants, so the resolve and the probes agree by construction instead of by a pin.

**The gain.** `LIGHT_GAIN` went up. The sweep it was picked from — clip fraction, flame and pool luminance and their
ratio, captured in DIG_VAULT at the standard pose — is recorded in this note; the finding worth repeating is that the
obvious compensation does not exist. A brighter pool makes the flame stop out-reading what it casts, and `EMISSIVE_GAIN`
cannot answer: raising it substantially moved the sprite by a couple of luminance levels and no more, because the ink is
already clamped by the surface-format scene target. So pool brightness is bought directly out of that margin — the
flame-to-pool ratio falls steadily as the gain rises and reaches parity not far above the shipped value, where the wall
behind the torch reads as bright as the flame and the picture inverts. Clipping rises over the same sweep, though it
stays well under what R22 removed. The shipped value is the last notch before that turns over.

This is the third constant now standing in for the missing tonemap, after `LIGHT_NEAR` and `EMISSIVE_GAIN`. The honest
fix remains an HDR scene target and one tonemap on the composite; until then every torch tuning is spending the same
fixed 0–1 range.

### R27 — one shadow ray, all the torches

User report: flicker still there away from torches, worst at night, worst with several torches near each other, **only
while moving**, and even a slow walk shows it — sometimes as thin shards stretching away from the torches for a frame or
two.

R20 built the shadow ray and picked the **argmax**. That covers DIG_VAULT, which holds one torch. Every other emitter
kept the gather's direct-hit term — `alb * emit * LIGHT_GAIN`, several times a display whose white is 1.0, on odds of
striking one small cell with a cosine ray, which are vanishing. Two estimators for one quantity, reconciled for exactly
one light, and R20's own note says so.

**The instrument came first, because DIG_VAULT structurally cannot show this**: a sealed room small enough that every
surface is inside `LIGHT_RANGE` and holds the one emitter count the argmax covers completely. `DIG_TORCHES=<n>` is an
open courtyard with the cluster well out from the camera and floor running past `LIGHT_RANGE`, so `n` is a knob with the
geometry fixed. `DIG_TORCHES_FAR=<n>` parks emitters far BEHIND the camera, where they can only change the light list's
LENGTH. `DIG_TORCHES_WALL` screens the cluster so emitters occlude each other.

**The metric is a same-pose diff, and it is the reusable part.** `--walk` oscillates on a fixed period and
`--fixed-dt
1/60` makes that exactly 90 frames, so frames N and N+90 sit at the IDENTICAL camera pose after 90 frames of
continuous motion: the diff holds no parallax at all, only the temporal noise of a moving accumulator. Frame N vs N+1
cannot separate the two — a 0-torch walk reads far higher there than here, so the consecutive-frame number is mostly
parallax. Repeat runs diff at **zero**.

**Attribution** (8 torches, night, walking, same pose; mean |Δ| and the far quarter of the frame), each build against
the shipped one:

| build                                           | effect on flicker                                         |
| ----------------------------------------------- | --------------------------------------------------------- |
| `DIG_STUB=ginee` — no NEE, all 8 on the lottery | worse, in both mean and far band                          |
| `DIG_STUB=gigather` — no per-pixel gather       | far better                                                |
| `DIG_STUB=gigather,ginee` — probe cache alone   | better still                                              |
| every emitter hit suppressed (one-line probe)   | **the best of the set, and by far the flattest far band** |

The gather's emitter hit was the overwhelming majority of the flicker and nearly all of the far band, for a small share
of the scene's light. And it tracked the emitters the argmax does NOT cover: flicker climbed monotonically over
0/1/2/4/8 torches, and steeply.

Every clause of the report is that one term:

- **only while moving** — the hit is absorbed by history depth alone. At rest `n` reaches `N_MAX`; the instant the
  camera moves it clamps to `N_MOVING`, so each hit lands an order of magnitude harder and decays in a fraction of a
  second instead of many. Measured at rest against walking, the gap is nearly two orders of magnitude.
- **even a slow walk** — `still` is a BINARY test at `TRAVEL_EPS`. There is no ramp: a camera creeping barely over the
  threshold gets the identical `N_MOVING` regime as a sprint.
- **away from the torches** — the term is RADIANCE, so it does not fall off at all. Its mean drops as 1/d² and its spike
  does not, so relative noise grows LINEARLY with distance; past `LIGHT_RANGE` nothing NEE-samples and every torch is
  pure lottery.
- **at night** — measured against band luminance, the term is an order of magnitude worse at `--tod 180` than at noon.
- **thin shards for a frame or two** — `sample2`'s R2 rotation is GLOBAL to the frame and `cosine_hemi` maps `u.x` → 1
  to grazing rays, so on some frames whole correlated regions fire near-horizontal rays together, and a grazing ray
  reaching a torch far across the courtyard still returns the term at full strength. Over frames 900–907 the typical
  consecutive pair moves modestly with a scattering of pixels flashing; **one pair in that run spikes several times
  higher, with an order of magnitude more pixels flashing**. That spike is flat with the gather stubbed, and flat with
  no torches.

Three suspects died by measurement rather than by argument, recorded so they are not rebuilt. **Not gi_probe's uniform
light pick** — `nee()` draws one entry uniformly over the whole list and reweights by ×count with no range cull, which
looks like a variance bomb as the list grows; adding a batch of emitters that cannot light one visible pixel (the
overlay's `emit` count confirmed they were resident) did not move flicker at all. **Not the deterministic NEE ray** — a
small fraction of the total, and it does not produce the burst. **Not history depth** — R14's own `N_MOVING` sweep
already bottomed out, and the rung past it brings the resampling smear back.

**The fix is one estimator instead of two.** `path_radiance` returns nothing for an emitting cell, and the direct term
is RESAMPLED over every in-range emitter rather than picked. `resample_light` is Talbot-style RIS in its exact discrete
form: the loop already visits every entry in the list, so there is no candidate subset to draw — one weighted-reservoir
pass selects light i with probability exactly `p_i / Σp`, and the contribution weight is `Σp / p_sel`. The target
function is R20's own score, `emit·ndl/max(d², LIGHT_NEAR²)`, which makes the estimator **exact rather than merely
unbiased** whenever the in-range emitters share a tint and are mutually visible: `f_i = albedo · LIGHT_GAIN · p_i`, so
`f_sel/p_sel · Σp = albedo · LIGHT_GAIN · Σp` and the drawn index cancels. Every torch in the world is one class today,
so in the open the pool is as steady at n = 1 as the argmax was, while carrying ALL of the light instead of the
brightest slice of it.

One uniform PER RESERVOIR UPDATE is load-bearing. Reusing a single draw for every candidate is the tempting shortcut and
it is wrong — the tests become nested events on one variable, so the last candidate keeps its exact marginal while
everything before it is squeezed (three equal weights land 1/2, 1/6, 1/3 instead of thirds). An LCG advanced per
ACCEPTED candidate costs a handful of ALU, on the in-range lights only. Checked against a large-trial reference
implementation: selection matches `p_i/Σp` to within Monte Carlo error, and the estimator with visibility recovers the
analytic truth.

Two things had to come with it:

- **gi_probe's emitter hit went too**, on the same argument (`nee` covers every emitter there as well), and it was
  carrying a stale calibration — a bare literal equal to the pre-R26 `LIGHT_GAIN`, invisible to
  `resolve_and_probe_agree_on_light_calibration` because that pin compares the two shaders' NAMED consts and a bare
  literal has no name to compare. The cache's own flicker HALVED rather than rising, which is also what retires the
  worry that the ×count pick would start to bite once it was the only path.
- **An emitting partial cell stopped being a hit whole.** That rule was never about occlusion — it existed so a gather
  ray would reliably strike the torch's cell and collect its emission, because letting rays through collapsed the
  vault's pool. With the emission gone it was doing only the thing its own comment said it must not: casting a full-cube
  shadow from a post a fraction of a cell wide, into the NEE rays of every other torch in a cluster. Worth a further
  drop in both mean and far band at 8 torches, with 1 torch unmoved.

**Measured**, same pose, night, walking, over 0/1/2/4/8 torches: the shipped build's flicker climbed steeply with the
emitter count in both the mean and the far band, and R27's barely moves off its 0-torch value at any count. The far band
is the line that matters — it was linear in the emitter count and is now flat. Frame luma is unchanged at 0 and 1 torch
and rises with the count thereafter. Noon improves by the same shape, so this is not a night-only fix.

**The room gets brighter, and that is a look call, not a bug.** It brightens in proportion to how many emitters the
argmax was dropping — most at 8 torches, less at 4 and 2, and **not at all at 1**, which is the parity that matters,
since with one light RIS degenerates to exactly the argmax. Seven of eight torches were previously delivering their
light through a channel whose hit probability at that range is vanishing, which a moving history that short cannot
collect: unbiased over an ensemble, and in practice simply absent while walking. `LIGHT_GAIN` is the sanctioned knob if
the pool now reads hot (R26 above carries the sweep it was chosen from), and note its old calibration anchor — "the
brightness the direct-hit term already gave the emitting cell" — no longer exists.

**Parity on the existing capture envs.** DIG_ROOF (no emitter) is untouched. DIG_VAULT moves visibly and **loses luma**:
with one torch the resampled NEE *is* the old argmax, so all of that is the gather's and the cache's emitter hits going
away with nothing to replace them — the vault is the one scene where this change is pure subtraction. DIG_SHAPES moves
about as much at **unchanged luma**, a redistribution rather than a shift, and it moves at all because it holds two
torches (it is a slab/stair scene, which is not where you look for emitters — checking rather than assuming is what
turned a "no-emitter scene changed?!" scare into a one-line explanation). Its same-build repeat floor at 1500 frames is
**zero**.

**Cost**, interleaved brackets at `--gi-scale 1 --size 1280x720 --draw-dist 16`, an artificially heavy config chosen so
the frame is longer than any refresh interval: DIG_VAULT with its one torch got marginally FASTER in both frame time and
GI trace, and the eight-torch cluster got clearly slower, its trace taking most of that cost — which scales with the GI
resolution, so the shipped divisor divides it. Some of that is work the fix BUYS rather than wastes: a NEE ray no longer
terminates early against a neighbouring torch's false full-cube occluder, so shadow rays in a cluster now actually run
their length. Deferring the per-candidate `sqrt`/`gi_light_margin` to the winner measured better, but by less than the
within-build spread of that bracket, so treat the deferral as obviously-correct work removal rather than as a measured
win.

**Two measurement traps this round, both worth the standing list.** `--display N` is new and it is not a convenience: on
a multi-monitor desk the window opens wherever the desktop last had focus, so an unpinned bracket compares one screen's
refresh rate against another's — and a display-paced frame lets the GPU downclock, which inflates every timestamp in the
CSV together. The first, unpinned bracket read an `avg_ms` for each build that was exactly the refresh interval of
whichever screen it landed on, and had `gpu_gi_denoise_ms` swinging wildly **within one build**, on a pass this change
does not touch. `refresh_ms` in the CSV is the tell. And frame N vs N+1 is not a flicker metric — use the 90-frame
same-pose diff above.

**What is left, and the number that would justify the next step.** The draw is proportional to UNSHADOWED contribution,
so a point that can only see some of what it samples gets a Bernoulli on the pool total — the one thing the target
function cannot price. `DIG_TORCHES_WALL` exists to measure it rather than argue it: 8 torches screened in half read
**0.103 / 0.099** against 0.070 / 0.035 in the open. Real, and still 4× better than the shipped build's OPEN case. The
fix is a temporal reservoir (ReSTIR DI — reproject through `prev_vp`, which the resolve already does, with an M-clamp
around 20); it is deliberately NOT built here, and **0.103 is the number to beat**.

### R28 — a torch in a hand is a torch

A Torch `emits` and always has, but only ever *as a placed cell*: `GiDraw::emitters` is keyed by `ChunkPos` and written
only by `upload_chunk`, so there was no path by which a light could exist anywhere but a voxel. Pick a torch up and it
stopped being a light.

**The governing decision, and it is what makes this small: no carve-out.** A carried torch is an ordinary entry in the
ordinary light list. `flush_lights` folds a per-frame `carried` slice into the same candidate iterator as the chunk
emitters, under the same reach filter and the same `MAX_LIGHTS` selection — and **the two NEE passes differ by one
question:** `gi_light_margin_of` asks whether the entry is carried, which is a flag bit in its packed `w` word
(`GI_LIGHT_CARRIED`, mirrored by `gi::LIGHT_CARRIED`).

That one exception is not a softening of the rule, it is the rule applied honestly. The margin is how far short of a
light its shadow ray stops so the light's OWN CELL cannot occlude it, derived from where in that cell the light sits —
and a carried light has no cell. Left to the general formula it lands on the degenerate maximum, because the origin
hangs a fixed distance above the feet and a carrier standing on a block top gives `frac(y) == 0.0`: wider than the worst
any placed emitter, a torch's flame included, can produce. The player's capsule is `RADIUS` across, so a wall they are
brushing falls entirely inside that skip — the floor on its far side lit through solid stone. The carried answer is
**zero**, and it is exact rather than tuned: the origin is on the carrier's own capsule axis, so its cell is non-solid
by construction and a wall half a block away is supposed to block it. Every consumer of that list picks a carried light
up for free: the probe NEE (so it bounces), the resolve's per-pixel reservoir (so it throws hard shadows), the water
surface's per-vertex gather, `players.wgsl`'s remote-player shading, and `cam_light` (so it lights your own hand and the
spray). The class byte is `gi_class(make_cell(Torch, 0))` — the *same* synthetic shape class a placed torch uses, so
tint, level and R26's flame offset come from one LUT row and the two lamps cannot drift apart.

**Where it hangs.** `gi::carried_origin(eye)` hangs it a fixed drop below the eye — a function of the carrier's POSITION
and not their facing. Two properties fall out and both are load-bearing. The cell is provably non-solid, because the
carrier's own capsule occupies it — so brushing a wall can never bury the light and put the room out. And **mouse-look
does not move it**, so a player standing still and turning is not a moving emitter: nothing invalidates, the screen
history climbs back to `N_MAX`, and a room lit from a hand settles as deeply as one lit from the floor. A hand-anchored
origin swings through half a block on every flick and would hold the near field permanently short. The cost is that the
light does not sit exactly where the item is drawn — a fraction of a block out for a remote, which is nothing against
`LIGHT_RANGE`, and nothing at all in shadow terms, since players are not in the voxel window and a carrier never
occludes their own light anyway.

**Motion is expressed as re-placement, not as a new mechanism.** Travel a probe spacing and the light fires the same
`gi_invalidate` a block edit fires: probe ages reset, so the bounce field re-traces at α = 1.0 on its next visit instead
of decaying from a stale value at the ladder's steady-state alpha, whose half-life is seconds, and the resolve caps
screen history nearby while it does. The trigger is a DISTANCE since the last fire, deliberately **not** a cell crossing
— a crossing test is phase-dependent, and a player resting on a cell boundary with the sub-block wobble the tick lerp
and the decaying reconciliation offset both produce would fire a sphere every frame. Distance has no such failure mode,
needs no hysteresis, and handles a teleport. `CARRY_INVAL_STEP` is exactly one C0 spacing, so a carrier can never outrun
a probe cell between fires; only carriers inside a bounded radius spend a sphere at all, because past that the wake is
unreadable.

**Going out is a re-placement too**, and it is the half that is easy to forget. Put a torch away and `flush_lights`
drops the entry the same frame, so the direct pool dies at once — but the probes keep its bounce and decay it a sweep at
a time, so without a fire at the position it left, the room stays lit by a torch that is gone for seconds.
`Carried::finish` therefore invalidates for a carrier who has STOPPED emitting as well as one who has moved, which
covers swapping items, a remote disconnecting, and a remote falling out of the snapshot. Digging a placed torch has
always invalidated through `take_gi_edits`; the whole thesis is that the two lamps behave alike.

**The bookkeeping is banked only once the sphere is actually queued** (`Carried::commit`, run after the drain rather
than inside `finish`). `pending` is consumed inside the renderer arm, so recording a fire at queue time meant a frame
with no renderer — device loss, or the wait for the GPU at startup — logged one that never happened, and the carrier
then sat stale until it had walked another whole step.

**The screen-cap slots are shared, and the two streams evict their own.** Draining the probe queue also pushes each
sphere into `GiDraw::resolve_inval`, which holds a fixed handful for `RESOLVE_INVAL_FRAMES` and is what stops a resting
pixel at `N_MAX` admitting a lighting change at 1/`N_MAX` a frame. A world edit fires once; a carried light fires
several times a second at `WALK_SPEED`, so under a plain newest-wins ring one walking torch-bearer holds nearly all the
slots continuously and turns them over faster than an edit's own window — a dug-out torch's pool fading back in over
seconds, which is precisely what `RESOLVE_INVAL_FRAMES` exists to prevent. Eviction is therefore per-kind: each stream
recycles its own oldest slot, so an edit that wins one keeps it for its full window however far anybody walks, and no
second constant had to be invented.

**`flush_lights` stopped sorting.** Its own doc already said order never matters to a reader — the sort existed only so
the truncation kept the nearest. That was free while the list was static and is not now: a carried light moves, so the
flush runs every frame anyone walks, over every emitter within `lateral_max`. Under the cap it now goes out unordered;
over it, `select_nth_unstable_by` partitions in O(n). No reservation for carried lights and no `MAX_CARRIED`: the
distance rule already does the right thing at both ends, since your own light is half a block from the camera and cannot
be the one dropped, while a distant player's torch losing its slot to nearer emitters is the correct answer under the
rule that governs everything else.

**Measured, `DIG_VAULT_DARK=1 DIG_SCREEN=torch` at 960×540, tod 45, 600 frames.** Mean interior luminance is more than
double the same room with a non-emitting item in hand — the carried torch is the room's light. Clipping, R22's metric: a
smaller fraction of pixels with any channel at 255 than R22's placed-torch baseline, and a small share of those are
actually white. The near-field disc a light at chest height throws sits entirely inside `LIGHT_NEAR` where irradiance is
flat, and it reads as a warm pool rather than a saturated ellipse — carrying a torch clips *less* than standing next to
one. `cam_light` does not blow out either, and the reason is structural: a probe stores what its RAYS find, so a torch
reaches the hand as its bounce off the floor, never as `emit·GAIN/LIGHT_NEAR²`.

**What is left, and the measurement that would justify the next step.** The screen accumulator is a per-world-point
average, which is exactly why a placed torch has zero lag at any history depth: the irradiance at a world point does not
change as you walk past it. A carried light breaks that premise — the signal at a world point moves at the carrier's
speed — so the direct pool trails by the history depth. Walking, `still` already clamps to `N_MOVING`, and the
steady-state attenuation is `x·eˣ·E₂(x)` with `x = d/(v·N/60)`: at `WALK_SPEED` the pool reads well under truth, and
increasingly so the nearer the surface. This is the one place "just another torch" is not literally true, and it is a
bias rather than the variance every previous round measured — so the instrument is a low-passed `--fly 600` against a
FIXED converged `--warp` reference, banded by distance, not the 90-frame same-pose diff. If it reads badly the minimum
fix needs no new binding: `rz.inval[si].w` is a radius, so a carrier sphere can ride it with a NEGATIVE one and cap at
an `N_CARRIED` short of `N_MOVING` — with a second uncapped local for the sun's alpha, since `a_sun` shares that counter
and R14's sweep already rejected a cap that short for daylight shadows.

**And the one that R28's own capture could not see.** The resolve's reservoir gates the WHOLE in-range pool's irradiance
on the visibility of the single light it drew, which is R27's known residual — but a carried light skews the draw hard,
because `score` clamps `dist2` at `LIGHT_NEAR²` and a light half a block from the eye takes that clamp with a high
`ndl`. A wall torch several blocks out scores lower by the square of the distance ratio and wins the draw only
occasionally; twice as far, several times more rarely again. Its irradiance is still paid in full every frame — `wsum`
is a plain sum and unaffected — but its OCCLUSION is now sampled that rarely, and because the carried light is itself
never occluded (nothing sits between an eye and a point half a block below it) the usual answer is "unshadowed". So
while you hold a torch, other torches' shadows wash out rather than get noisy. Unbiased, and it converges — but at
`N_MOVING` while walking that is a handful of effective samples.

`DIG_VAULT_DARK` cannot measure this: it is a room with exactly one emitter, which is why it shipped unseen. The capture
is `DIG_TORCHES=8 DIG_TORCHES_WALL=1 DIG_SCREEN=torch`, screened emitters plus a carried one, read against the same
scene with a non-emitting item in hand — R27's 0.103 is what it has to be compared with, since this is the same
estimator and the same fix (a temporal reservoir, M-clamped, reprojected through `prev_vp`). It is deliberately not
built here either; what is new is only that the carried light makes R27's residual reachable in ordinary play rather
than in a contrived screen.

## Goals

- One mechanism lights everything: sun, sky, and torches are all radiance sources seen by the same rays. No shadow maps,
  no per-light machinery beyond a light list.
- Amortized over time, never in one frame: probes converge over fractions of a second with hysteresis; world edits and
  sun movement propagate as re-convergence, not recompute.
- GPU-resident: the web client is single-threaded — the CPU's only jobs are classifying edited chunks and byte-budgeted
  uploads.
- Low resolution with blending: coarse probes + a half-res resolve + temporal smoothing is the look; crispness comes
  from the two per-pixel rays — the sun ray, and the final gather. Baked AO is gone; contact darkening is what the
  gather ray does when the thing it hits is close.
- Fits WebGPU baseline `Limits::default()` — no feature/limit negotiation.

## Non-goals (v1)

- Specular GI / reflections (SSR stays as-is for water).
- Shadows beyond the voxel window (analytic fallback, mostly under fog).
- ~~Sub-block emitter geometry: a torch lights from its cell, as a voxel.~~ Retired: R26 gave shaped emitters a real
  origin inside their cell, and R28 gave a carried one no cell at all.
- Colored transmission through water beyond a scalar transmittance.

## Literature

Majercik et al., *Dynamic Diffuse Global Illumination with Ray-Traced Irradiance Fields* (JCGT 2019); *Ray Tracing Gems
II* probe chapters (RTXGI scaling, relocation and classification); NVIDIA RTXGI-DDGI; DDGI-Minecraft (Vulkan);
*Real-Time Rendering* 4th ed. ch. 11 (ambient-cube basis). Two deliberate deviations from stock DDGI, both possible
because we own an exact voxel occupancy texture: (1) no octahedral visibility payload — leak suppression is exact short
DDA occlusion at resolve time plus probe relocation; (2) the probe payload is a 6-axis ambient cube, not N×N octahedral
— blocky world, one texel per axis, trivial sampling.

## Data model

**Voxel window** — R8Uint 3D texture of GI-class ids, one texel per block, XZ-toroidal at chunk granularity, Y spans the
full world column (an up-ray always reaches sky; the window never scrolls vertically). The per-tier dimensions and their
memory live in `gi.rs`: desktop widest, web narrower, iOS narrower still. GI-class mapping is a pure function in
`digblock-core` (headless-tested): air has its own class, water is translucent, `emits`/`glows` blocks map to emissive
classes. Shaped cells were "opaque, the same v1 approximation fluid makes" until R16: they now classify per (id, state)
into synthetic PARTIAL classes from `SHAPE_CLASS_BASE` up, each carrying a two-box cover of the shape's collision boxes
(box 0 = the first collision box exactly, box 1 = the AABB union of the rest — over-blocks only, never under, worst case
the old full cell), and a state with no collision at all (an open door) classifies as air, which is what lets light
through a doorway. A chunk re-uploads as one `write_texture`; scrolling uploads only entering slabs.

**Material LUT** — one storage-buffer row per GI class: albedo rgb, the opacity class (air / water / opaque / partial)
and the cast-light level in the first words, the partial class's two packed sub-boxes (in twelfths, zero meaning no box)
in the last. Albedo is the per-layer mean color, computed where the atlas upload already iterates the layers; synthetic
shape classes borrow their base block's albedo and light. Rays never touch the block atlas. `gi_boxes_hit` (gi_dda.wgsl,
so the probe kernel and the fragment marches share one definition) tests the boxes against the ray's in-cell [t_entry,
t_exit] span — the ESVO-contour / SEUS-PTGI pattern.

**Coarse occupancy** — a second R8Uint 3D texture, one texel per coarse region; the byte carries an any_solid and an
any_emissive bit for that region, computed CPU-side at classification. Rays in empty space skip a whole coarse region
per load.

**Light list** — up to `MAX_LIGHTS` emitters within `gi_lateral_max` of the camera: `pos` plus the GI class byte
bit-cast into `w`, one storage buffer. Tint and cast level come from that class's material-LUT row, so nothing about a
light's *look* rides the list. Rebuilt from the per-chunk emitter lists kept at classification, plus (R28) the lights
players are CARRYING. Exists so next-event estimation makes a single torch voxel stable at the probe pass's ray budget
instead of a firefly.

**Probe cascades** — two toroidal camera-following grids of 6-axis irradiance probes: C0 fine and near, C1 several times
coarser and correspondingly wider, each a fixed grid whose dimensions (and iOS's smaller pair) live in `gi.rs`; coverage
is spacing × grid size. Payload: one rgba16float 3D atlas per cascade, dims (nx, ny, nz×6) — axis-major Z slabs,
hardware trilinear with a half-texel Z inset so slabs never bleed. Ping-ponged: the update compute reads last frame's
atlas (that read IS the infinite bounce) and writes the new one; a full copy precedes the amortized write. A r32uint
flags texture per cascade carries, in order, the probe's age, a dead-in-solid bit and a relocation code (the probe
centre nudged into air, at most a block). Beyond C1: analytic fallback (ambient dome × face weight + lambert sun,
visibility 1), faded in over C1's outermost band.

## Passes (frame order, RTGI on)

probe copy → probe update (compute, per cascade) → invalidate (compute) → depth prepass (opaque solids, position-only) →
depth blit to R32Float (existing pipeline, moved earlier; one R32F serves both GI resolve and water) → GI resolve →
scene → water → viewmodel → overlay.

**Probe update** — one workgroup per probe. Spherical-Fibonacci ray set rotated by a per-update hash. Per ray:
coarse-skip DDA → on hit, LUT albedo/emissive + one sun DDA from the hit + a 6-axis gather from last frame's atlas; on
miss, sky radiance from the frame's zenith/horizon gradient. Plus NEE against the nearest few list lights (one short DDA
each). Scheduling is a permuted round-robin over probe index — responsiveness comes from the alpha ladder, not a
priority structure.

**GI resolve** — fullscreen at half res (quarter on iOS; web capped to a fixed maximum), MRT. World position from R32F
depth + inv_view_proj; face normal from depth derivatives (exact on a blocky world). Per pixel: the 8 surrounding C0
probes — 3 axis fetches each (n² ambient-cube weights) + a short DDA occlusion probe→pixel — falling through to C1 (its
occlusion capped at a few coarse steps) then analytic; plus one sun-visibility DDA. Out: rgba16f (indirect rgb + view
depth) and rgba8 (sun visibility + packed normal code).

**Scene shading** — chunk.wgsl: the emissive bit's early return unchanged; else
`tex * ao * (light_color * sunvis *
lambert + indirect)` via a 4-tap depth/normal-aware bilateral upsample of the GI
buffers. Water: manual probe gather at the surface world pos in fs (no new varyings) + its own capped sun DDA;
underwater dimming is the sun ray accumulating transmittance through translucent voxels. Players: per-fragment gather +
1-tap sun occlusion. Viewmodel/spray: a 1-invocation compute samples the field at the camera into a small buffer copied
into their UBOs.

## Budgets and cadence

The per-tier budgets live in `gi.rs`, and each is a trade worth naming rather than a number worth copying: **probes per
dispatch** and **rays per probe** together set how long a full sweep takes — more of either converges faster for more
GPU time every frame; the **resolve divisor** trades reconstruction detail against the cost of the two per-pixel rays
(web additionally caps the resolve's absolute size); and the **per-frame GI upload budget** trades how fast new terrain
enters the window against a hitch on the frame that pays for it. Desktop takes the largest of each, web the smallest
sweep, iOS the coarsest resolve.

- Hysteresis alpha by probe age: a freshly reset probe takes its estimate whole, and the alpha steps down by rungs to a
  steady-state floor.
- Invalidation: a bounded number of CPU edit spheres per frame (a radius per cascade, wider for the coarser one) reset
  ages; a teleport zeroes every age and the alpha-1 sweep hides inside the streaming fill.
- Sun: the ray direction is quantized, so it advances in steps a few seconds of `DAY_SECS` apart; steps do NOT
  invalidate — shadows glide across a step inside the hysteresis ladder's own settle time. The smooth un-quantized
  direction still drives `light_color`/lambert.
- Convergence gates — these are ACCEPTANCE CRITERIA the tier budgets are tuned to meet, not a reading off them, so they
  are stated as targets: a placed torch visibly lights ≤0.5 s desktop / ≤1.5 s web, and settles ≤3 s.

## Invariants

- The voxel window is DERIVED data: never serialized, never on the wire; device loss rebuilds it by re-upload + age
  reset (drill-tested).
- Classification is a pure function of the cell value — headless tests pin it.
- Everything the rays read (window, LUT, coarse, light list, atlases) lives on the GPU; the CPU never reads probes back.
- Flag-off builds are bit-identical to the old system (P0 bench gate).

## Test pins (grown per phase)

- P1: classification table (air/water/emissive/shaped→opaque), coarse-byte derivation, window addressing (toroidal
  chunk→texel), dirty-set feed from set_cell/arrival/evict.
- P2/P3: naga-validated shaders (existing harness); frame-layout test updated with the GI uniform fields; capture A/Bs —
  DIG_VAULT (sealed room: dark, then torch-lit), DIG_ROOF (a morning sun slants under a wall-less roof), tod 0/45/180.
- P5: bench with a fixed-tod converged steady-state segment.

## Phases

P0 compute infra + this doc + `rtgi` flag (gate: flag-off bench parity) · P1 voxel window

- LUT + light list (gate: `DIG_GI=window` debug raymarch capture) · P2 probe cascades + update/invalidate computes
  (gate: probe visualization capture) · P3 depth prepass + GI resolve + chunk.wgsl (gate: A/B captures; USER LOOK
  SIGN-OFF) · P4 water/players/ viewmodel/spray/underwater · P5 perf + metrics + bench + web verify · P6 remove the old
  system — DONE at M1 (shadow.rs, shadow.wgsl, core light.rs, mesher light plumbing, cascade VPs, the pinned tests that
  went with them, LIGHT-DESIGN.md, the flag itself).
