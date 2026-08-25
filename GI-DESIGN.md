# GI-DESIGN — probe-traced global illumination

Status: **M4 landed on `feat/rtgi`.** RTGI is the only lighting path and there is no flag. The raster stack it replaced
is DELETED, not dormant: shadow.rs/shadow.wgsl and the cascades, core/light.rs and the Stage-B BFS voxel light field,
baked per-vertex AO, all fog, the hand-tuned water look, and per-block tint jitter. Their design doc went with them. The
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
  the probe field per fragment through gi_common.wgsl — and players additionally run their own NEE against the emitter
  list (R33), because the resolve's half-res direct term only covers prepass surfaces: `sum_lights`, a ray at each of
  the brightest few, rather than the resolve's one resampled ray, because nothing filters a body (R34). Spray and the
  viewmodel read a small camera-light buffer a 1-invocation compute writes.
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
  / 2`), so the real gi/full ratio has to be plumbed in first. R36 built the
  measurement path and the ray-budget lever (`?stub=`, `?giiters=`, the emitter bed) without moving a default — the
  re-measurement this bullet asks for now has its instruments, and R36 records the decision rule the numbers will be fed
  into.
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
  that leaves the window and is answered "fully lit" on no data, along a boundary centred on the camera. (Since R32 that
  no-data answer fades with depth below `SKY_FADE_FULL_Y`; at the surface, where this statistic was measured and
  accepted, it is unchanged.) That statistic is real and it is very nearly INVISIBLE, which is the part that took
  measuring. Building a window that genuinely covers the streamed radius (doubling the side AND lifting `engine.rs`'s
  upload clamp, which is what the earlier attempt missed) and holding the sun ray's reach as the only variable moves the
  image by mean 0.0169 — at the 0.007 repeat-noise floor. Distant terrain is mostly lit anyway, so "lit" is usually the
  right answer, and by the window's own half-reach the participating medium has faded the surface most of the way into
  the sky. What DID matter at that range was the final gather, and it cost nothing to fix — see `gather_max` in
  gi_resolve.wgsl. With the gather fixed, the whole remaining benefit of a full-coverage window is mean 0.0556, confined
  to the horizon band, bought for a QUADRUPLED window texture — **+75 MiB** (note `gpu_buffer_bytes` in the metrics
  counts mesh buffers only and does not see it) — and an initial fill slower in the same proportion, since the upload
  loop is byte-budgeted per frame. Declined on that trade, and those two figures are the trade. The earlier "doubling
  helps a lot and costs no frame time" note was **retracted**. That build could not have been measuring new data:
  `engine.rs` clamps the upload window to `draw_dist` independently of texture size, so the outer half of the texture
  was never written, and wgpu zero-initializes it — which `gi_cell` reads as air, the same answer the narrow window gave
  by breaking at its bound. Confirmed directly: with the debug view's `cam.w`-scaled depth cue neutralized so a diff
  means geometry alone, `DIG_GI=window` at the shipped side vs a doubled one differs by a literal handful of pixels in a
  multi-megapixel frame. The apparent win was several `side`-derived bounds moving at once against unchanged data, and
  most of it was `gather_max`. Whoever revisits this: the honest fix is not a bigger box. The window's Y extent is the
  full world height and most of it is empty air, so bounding Y is where the memory actually is; and the far field
  plausibly needs only coarse occupancy — a small fraction of a full-identity window's bytes even at the doubled side —
  rather than full material identity, since the gather ray only reaches `GI_GATHER_T`.
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
  the camera has never seen. What made this a QUALITY question rather than a bug fix is that the smear which motivated
  it was already closed — walk residual 0.922 → 0.578, flyover 2.84 → 2.08, for 0.14 ms — using SVGF's spatial half
  instead. Indirect should stay on the probes regardless — it is smooth and does not smear — so the cache would only
  ever have carried sun visibility. Reopen only if "converged while walking" becomes a requirement;
  denoised-while-walking is what exists today.
- **Glass touching a lit surface does not tint it**, and a report will arrive worded as **"stained glass barely tints
  the block it is sitting on"** — a washed-out window sill, or a glass floor colouring the ground under it far less than
  the same glass colours a pool cast across the room. The cause and the cost of fixing it are in R35's edge list; go
  straight there. Three smaller R35 edges live beside it, all of them the draw and the marches disagreeing on how many
  times a path owes `T`.

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

This matters more than a second data point: the accumulator smear measured several times larger on the flyover than on
the walk, and it was the WALK crop that showed it as a picture — the flyover's larger number is mostly disocclusion,
which is not a bug. Read the two together, and crop before believing a global mean: that artifact sat on one shadow and
the flat foreground diluted it severalfold.

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

## R13–R26 — compacted

Fourteen release notes' worth of reasoning came out; what stays is the finding of each, because the notes that follow
cite them by number and because a decline nobody can find gets rebuilt. `git log -- GI-DESIGN.md` has the bodies, with
their measurements.

### R13 — the cap read the wrong variable, and rest was not rest

**Rest is not rest.** The history fetch round-trips a world position through `inv_view_proj` and back through
`prev_view_proj`; against a far plane hundreds of blocks out, f32 returns a fraction of a texel of drift with the camera
dead still. That reads as motion to a travel cap and, at a history depth near `N_MAX`, compounds through the bilinear
fetch into a multi-texel streak in a STILL frame. Fixed by snapping the fetch onto the lattice below `TRAVEL_EPS` and
making `n` two-state — full history or `N_MOVING`, no useful middle, since off-lattice blur compounds at a rate the
sub-texel PHASE sets rather than the speed. `a_moving_pixel_always_gets_the_spatial_filter` pins
`N_MOVING < N_CONVERGED`.

**The harness trap this set:** `--fly` vs `--warp` compares against a reference the change under test can move. An
earlier no-snap form measured as a clean win only because it had blurred the reference to match. Anything touching the
accumulator is measured against a FIXED converged frame.

R13 left the standing question of whether the accumulator belongs in screen space at all; R14 dissolved it.

### R14 — the sun ray stopped being an estimate

Sun visibility needed a long screen history only because the penumbra was a 1-spp Bernoulli trial. R14 traces toward the
caster deterministically, records the occluder distance and reconstructs the penumbra SPATIALLY from it (the NVIDIA
SIGMA / FidelityFX pattern). Pieces that are still load-bearing: `gi_sun_vis` returns the DDA's own hit distance; the
cone offset is STATIC and hashed from a sub-block WORLD lattice, so a point asks the same question from every camera
(screen-hashed was built first and shimmered under mouse-look); sun history caps at `N_SUN`, so smear is bounded by
construction; `n` moved into the sun target's g channel above the face code (`packed_metadata_stays_exact`) because the
per-attachment budget was spent; `gi_atrous` gates per signal, and the sun's filter is RECONSTRUCTION so it runs at rest
too. `N_MOVING` roughly doubled — legal only once the sun stopped riding it — and its sweep is what later notes cite for
rejecting caps that short.

**Harness traps:** a seedless capture rolls a new world; the walk pair needs `--tod` frozen; `--frames` is the total
frame the screenshot fires at; consecutive `just bench` runs drift thermally, so only interleaved brackets compare.

### R15 — a reveal starts from the cache, not from noise

A disoccluded pixel restarted at n = 1 from its own 1-spp gather, which where indirect is the whole signal is speckle —
visible as a ghost that develops over the first frames of any reveal. It now primes its accumulator from the probe cache
at `N_PRIOR` virtual samples and lets the ordinary 1/n machinery decay the prime out. Two gates, both measured in after
the ungated version regressed badly: a grazing miss is not a reveal (a same-face tap that missed depth by a hair keeps
the unbiased restart), and the prime only runs where the sun is NOT the light. The prior is `gi_cache_at`, not the full
`gi_indirect` blend — priming is bursty, and a cave-mouth swing reveals a hundred thousand pixels in one frame.

### R16 — a slab shadows as a slab

Shaped cells classified opaque, so a slab cast a full-cube shadow and an open door blocked its own doorway. They now map
per (id, state) to synthetic PARTIAL classes from `SHAPE_CLASS_BASE` up, each carrying a two-box cover, and every march
clips the ray's in-cell span against them (`gi_boxes_hit`, in gi_dda.wgsl so all three marches share one definition). A
state with no collision classifies as AIR. R16 also made an emitting partial cell a hit-whole for the gather marches;
R27 retired that rule along with the direct-hit term it existed for.

### R17 — water stopped paying per pixel for a signal that varies per probe

Water shaded its indirect term by calling the full cascade blend per fragment at full resolution — the resolve only
survives that call at half res behind temporal history. The gather moved to the VERTEX shader: water quads are strictly
per-cell, so vertices sit closer together than the probe field and perspective interpolation loses nothing. The depth
prepass gained a position-only `vs_depth` sharing the colour pass's `water_world` decode, which is what the `@invariant`
/ LessEqual contract demands. Players keep the per-fragment gather deliberately.

### R18 — the sun march learned where the sky starts

Every water fragment's capped sun loop was spending its whole budget proving "open sky" with no early exit short of the
top of the world. The **sky ceiling** — the window's highest occupied cell y, scanned on the CPU at classify time —
makes a non-descending ray above it a proven miss in one float compare, and it pays in every march, not just water's.
The **super-brick skip** was built and deleted: checked before the brick it taxed every dense iteration, nested behind
it every empty one, and at terrain altitudes a cube that large almost always holds something — the world's occupancy is
a thin shell around sea level, so a cubic skip level asks the wrong-shaped question.

### R19 — the column field: built, measured, declined

The per-column max-height field R18 scoped got its trial and did not ship; the full patch is parked at the
`archive/r19-column-skip` tag. In both gi_common marches it made the resolve SLOWER — an underwater pixel's sun ray sits
below the water-inclusive column max for its whole climb and pays the dependent 2D load without ever skipping, the
super-brick lesson in a new hat. The one ray that profited was water's capped sun march, and there the gain was
correctness rather than milliseconds: that budget TRUNCATES into "lit" against far banks. The look change was declined
at sign-off, so the truncation stands as a KNOWN, accepted artifact — **distant shorelines render slightly
un-shadowed**. If a player reports it, start from the parked branch.

**Two harness traps, since retired by `--fixed-dt` (R21):** physics ticks ride the wall clock, so two builds of
different speed reach `--frames N` with the spawn settle in different states; and a reflection streak shifted by ripple
phase does not cancel under a low-pass, so water-band diffs need eyes on crops rather than thresholds.

### R20 — torch light became a shadow ray, and the support block lit up

Torch light arrived per pixel only when the 1-spp gather geometrically struck the torch's cell — a high-magnitude
Bernoulli trial no history depth can drown — and the support block could never win even that lottery, because every
march steps before it tests and a ray leaving the top face starts inside the torch's cell. R14's move applied to local
lights: the resolve binds the same light buffer the probe pass samples, picks the dominant in-range emitter by
`emit·ndl` over near-clamped inverse-square distance, and fires ONE shadow ray stopped short of its cell. The gather's
emission term for that cell is suppressed whether or not the ray connected. `LIGHT_RANGE`/`LIGHT_GAIN` became pinned
twins across both shaders (`resolve_and_probe_agree_on_light_calibration`). R27 replaced the argmax with a reservoir.

### R21 — water was marching two rays whose answers were multiplied by nothing

The measurement round. `DIG_STUB` (a uniform-branch mask removing one shading term per run, so one build attributes a
whole frame) plus `--pos` and `--fixed-dt` — the latter took the DIG_RIVER same-pose repeat floor to essentially zero,
retiring R19's traps rather than documenting them again. Attribution ranked water at nearly half the frame, dominated by
the per-fragment sun DDA and the SSR march. **Both were being multiplied by ~zero**: water has no diffuse sun response,
so sun visibility reaches the pixel only through foam, glint and lip sheen, and `reflection_color` ran as a function
argument even where the mirror weight was pinned at 0. Both marches are now gated on their own contribution.

**The threshold has to be in RADIANCE.** The first cut compared bare blend weights; against an HDR sky with no tonemap,
a small Fresnel weight is a large error, and it put a ripple-banded difference across a whole basin.

### R22 — the torch out-lit its own flame

A hard-edged saturated ellipse under every torch: per-channel clipping shifting hue to the light's, not a bright
surface. Two stopgaps, both explicitly for the absent tonemap. `LIGHT_NEAR` grew, so the near field stays near the white
point while everything past it keeps exact 1/d². And an emissive-ink gain rides the ink's OWN luminance, quadratic in it
— a light source must out-read what it casts, and the weighting matters because a torch is one block of two materials: a
flat multiplier lifts the wooden post into glowing wood.

**The real fix stays on the backlog:** an HDR scene target plus one tonemap. It cannot simply be appended — the
offscreen scene colour uses the SURFACE format, so radiance is already clamped at the scene pass.

### R23 — the per-block tint came back, on the albedo this time

Reinstated without a vertex bit: `fs` already carries the world position and the flat face code, so the cell is
`floor(world − n/24)`, nudged half a sub-grid step along the face normal because on that axis the position sits exactly
on a cell boundary. **It multiplies albedo, never light** — the à-trous filter and the accumulator only see the lighting
buffers, so a per-block step written into the light is a signal they are built to smooth away; on albedo it lands after
the bilateral upsample where nothing can filter it. Emissive ink is excluded structurally, inside the non-emissive
branch. Calibration was a sweep: the smallest rung is invisible, the largest quilts, the middle ships.

### R25 — water wrote itself over the backdrop it had already absorbed

Deep water read grey rather than blue. Four candidates died to A/Bs before the decomposition found the term was outside
the shader: the water pass carried a second frame UBO whose alpha was a raster-port constant, and water alpha-blended
over the same backdrop it had already sampled and absorbed — re-adding an unabsorbed copy. Near shore that is invisible;
under deep water the transport takes the backdrop to black and the leaked raw seabed is the entire picture. **Water
writes OPAQUE now.** Alpha is the falling-sheet streak gate alone, sitting at exactly 1.0 on every still surface,
because a torn sheet genuinely is see-through and `thick` cannot know about the tear.

### R26 — the torch radiated from its handle, and the margin that hid it

Every emitter was flattened to its cell centre, which for a torch is the wooden post. Inside `LIGHT_NEAR` that costs no
brightness at all — irradiance is flat in there — so the only thing it moved was shadow geometry, which is why nothing
caught it. `gi::light_origin(class)` now answers per class: the cell centre for anything unshaped, the centre of the
emissive band for a shaped emitter, derived from the texture recipe and the shape's own height rather than chosen
(`a_torch_radiates_from_its_flame_not_its_handle`).

Raising the origin broke the NEE stop-short margin, which had been the cell half-diagonal plus slack — a sphere that
contains its own cell **only from the centre**. From flame height the lower corners fall outside it and the post
shadowed itself, as four cell-aligned wedges. `gi_light_margin` replaces the constant with the distance to the farthest
corner of the light's own cell, in the shared DDA so both passes agree by construction.

`LIGHT_GAIN` also rose. The finding worth repeating is that the obvious compensation does not exist: `EMISSIVE_GAIN`
cannot answer a brighter pool, because the ink is already clamped by the surface-format scene target. Pool brightness is
bought straight out of the flame-to-pool ratio, which reaches parity — the wall reading as bright as the flame — not far
above the shipped value. Third constant standing in for the missing tonemap, after `LIGHT_NEAR` and `EMISSIVE_GAIN`.

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
assumed fix was a temporal reservoir (ReSTIR DI — reproject through `prev_vp`, with an M-clamp around 20), with **0.103
as the number to beat** — R37 built it, watched the estimator work and the image not move, and reverted it. Read the
refutation there before reaching for it again: the residual is real, and the filters behind this term already spend it.

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
estimator and the same assumed fix (a temporal reservoir), which R37 built and refuted: its argument — the accumulator
is a longer filter than any M-clamp — covers this residual identically. What is new is only that the carried light makes
R27's residual reachable in ordinary play rather than in a contrived screen.

### R29 — the torch was a white stick

User report: the torch itself reads far too bright — not the light it throws, which is right, just the object.

R22's ink gain rides the ink's OWN luminance precisely so that a torch's flame takes it and the wooden post does not.
That reasoning holds only while the post is darker than the flame, and in this texture it was not: `torch_side` cut the
post from the light joinery timber, whose luminance sits within a hair of the flame band's own. The two therefore took
essentially the same gain, and both went past 1.0. The post came off worse, because a flame keeps a low blue channel and
still reads yellow once red and green clip, while a neutral pale timber clips on all three at once and renders pure
white. Photographed in DIG_VAULT the torch was a featureless white bar with a yellow cap — the post out-reading the fire
standing on it, which is the exact inversion R22 added the gain to prevent.

**No shader change.** The post is now cut from `LumberDark`, far enough below the flame that the same weighting finally
discriminates: the post keeps its grain and its silhouette, and the flame is the only part of the torch at the ceiling.
Nothing about what a torch CASTS is in this change — `emits`, `LIGHT_GAIN` and `LIGHT_NEAR` are untouched — and the
capture agrees: against the previous build at the same pose, every pixel outside the torch's own bounding box differs by
at most one level in one channel, which is run-to-run noise. `gi::EMISSIVE_BAND` mirrors the recipe's flame band, which
did not move, so R26's origin pin is undisturbed.

The rule this leaves behind for the next emissive material: **an ink texture is authored for fullbright, not for lit.**
Anything painted near white has no headroom left, and a gain that cannot be raised past the scene target's ceiling
anyway (R22, R26) then spends itself entirely on clipping instead of on contrast. That rule is not new — texgen's ore
recipe already darkens the stone BODY of the two glowing tiers for exactly this reason, and says so, because a glowing
block draws its whole face at full albedo and the contrast has to live in the texture. The torch simply predates that
recipe, and was never revisited when it landed. The lamps and the deep ore pair are dark enough to be safe today; a
bright one shipping later would fail the same way, and it would read as a lighting bug rather than an art one.

### R30 — the tools light the tunnel, and one of them answers to its wielder

User ask: the Essence Siphon and the Resonant Pick should glow — dimmer than a carried torch, green and blue
respectively, "enough to help them see what they're doing down deep" — with the pick brightening at its second rank, and
the pick reading as the upgrade of the two.

**Most of this is R28 being right.** A carried light is an ordinary entry in the ordinary light list, keyed off whatever
the hand DRAWS, so `emits` on the two tool rows is the whole of "tools cast light": no new path, no second origin, hard
shadows and bounce and a lit hand for free. Two things did not already exist, and neither is about carrying.

**The colour comes from the texture, because it already did.** The material LUT takes an emitter's tint from the mean of
its row's top layer — a torch glows its own warm flame. A tool row rendered nothing and so wore layer 0, which would
have made both tools cast whatever the first texture in the atlas happens to be. Each now wears a texgen orb of its own,
and that same layer is the emissive face of its geometry: **the glow you can see and the glow it throws are one authored
thing**, and there is no second place to change one without the other. Both orbs are authored dark for R29's reason —
they are drawn fullbright, so a pale one would clip on all three channels and cast white.

**A per-FACE ink bit** (`ItemFace::emissive`, riding above the texture index in the item vertex word, decoded by
viewmodel.wgsl and players.wgsl alike). The per-block `glows` flag is right for a torch, which is one material; a tool
is a shaded body with one lit part set into it, and a per-draw flag can only choose between a tool with no form and a
glow that isn't one. The icon path takes the same bit as "no directional shading", which is all fullbright can mean in a
projection with no light in it.

**Two per-ENTRY properties** (`Emitter::level` and `Emitter::whitened`, packed above the carried flag; a level of 0
defers to the class, which is every placed emitter, so their words are bit-identical to before). The pick answers to its
wielder at the rank that widens its swing — keyed on `unlock::pick_cells` reaching `PICK_PLANE` rather than on a second
threshold — and that is a property of the HOLDER, not of the class: two players holding the same tool share a LUT row
and must be free to cast differently. A remote's tool casts at its row's level, since their skills are not in the
snapshot.

**Why the rank whitens the light and not just raises it.** A saturated blue carries barely a tenth of its channels'
luminance, so a blue light can be pushed to the top of the level range and still leave a room dim and uniformly blue —
the first cut did exactly that, and read as *more blue* rather than as brighter. `gi_light_tint` pulls a whitened
entry's tint toward white before the gain, which buys most of the brightness back and takes the cast away from
blue-on-blue; the level it pairs with came DOWN accordingly. The tool's own INK is untouched, and that separation is the
point: the pick still reads blue in the hand and in the hotbar, while the light it throws goes pale — a hot source, not
a different tool.

Measured in the sealed dark vault at the standard pose, mean luma over the lit floor (saturation of the same patch in
brackets): torch 75 [0.48], siphon 37 [0.33], pick 41 [0.51] at its first rank and 62 [0.36] at its second. So the
resonant pick is half again its own first rank and the brightest thing a hand holds short of a torch, while the siphon —
halved on a look call, since it is an extraction tool that happens to glow rather than a lamp — sits at a pool around
the feet. The pick out-LEVELS a torch at neither rank now, but it would still light the room far less if it did, because
what an emitter delivers is its level times its tint's luminance — which is why the pin that holds "a tool is not a
torch, and the pick reads as the upgrade" lives in `texture.rs`, the one file that can see the rows and the pixels at
once, and multiplies the two through the same whitening the shader applies.

**What the mixed-colour room showed, and what it did not.** A torch on the floor and a siphon in hand light the same
vault warm at one end and green at the other, blending across the floor with no hue noise at rest — R27's residual (the
reservoir tints the whole pool with the one light it drew) is a smooth spatial gradient here rather than the flicker a
colour difference might have made of it. Worth re-checking under motion when a scene finally has several differently
coloured emitters within one `LIGHT_RANGE`; `DIG_TORCHES=8 DIG_SCREEN=siphon` is the capture, and `DIG_MINING=<level>`
(new here, since a level is not a pose and the pick's second rank is otherwise unphotographable) is what puts the pick
at the rank worth shooting.

### R31 — a vein is a surface, not a lamp

User ask: revealed ore should glow — dim at the shallow tier, brightest at the deep one, each tier its own colour — and,
in their own words, "ore that has no visible faces wouldn't be emitting light, so I guess these are surface lights
instead of point lights". That instinct is the whole design, and it is also what the literature says: emissive geometry
has been sampled as POLYGONS for as long as path tracers have done next event estimation (Q2RTX extracts "light
polygons" from its emissive surfaces; voxel path tracers treat an emissive voxel as an area source), and points are for
sources that really are points.

**Why it had to be a surface, concretely, and not merely tidily.** Most of a deposit is buried, and a buried point light
LEAKS: `gi_light_margin` skips a cell's worth of ray in front of the light so it cannot shadow itself, which is wider
than the single course of stone between a vein and the tunnel next to it. And the resolve's NEE reservoir is a pass over
the whole light list PER PIXEL, so a thirty-cell pocket would spend thirty iterations of every shaded pixel on a source
that mostly delivers nothing. Both arguments point the same way, which is the comfortable kind of design decision.

**The mesher builds the lights, which is the part worth remembering.** Not `gi::classify_chunk`, the obvious home: that
reads the raw chunk store, and the store holds Stone where a vein is, because ore is derived and never written. The
mesher has already stamped the ore ids into its padded array — and already reverted every tier the player has not
levelled into — and is already deciding, face by face, what is exposed, with the neighbour shell in hand so a seam
across a chunk border does not go dark down one column. So BOTH halves of "if the player can see it and it has been
unlocked" cost nothing, and a tier lighting up the moment its reveal level is crossed is free, because crossing one
already re-meshes. The exposed faces are greedy-merged per slice into rectangles and ride out on `MeshData::lights`, the
rail `impacts` already established for spray emitters.

**The light entry is now a pair of vec4s**, the second carrying the extents and the facing, with `GI_LIGHT_QUAD` saying
which kind it is. Punctual entries leave it clear and their second half zero, so a torch's word is bit-identical to what
it was. `gi_light_sample` in the shared DDA prelude draws the point and returns what the sample is worth — 1 for a
point, `cos(theta_l) * area` for a rectangle — so the two NEE passes cannot price the same seam differently, and the
cosine at the light is a real gain a point could not have: a seam lights the tunnel in FRONT of it instead of spilling
around the corner of the rock it is set into. `gi_light_margin_of` answers only the slack for a rectangle, which is the
case that makes the whole margin look like the workaround it is: the sample point is already on the rock face with air
in front of it, so there is nothing of its own to skip.

The reservoir scores candidates from the rectangle's centre and draws a point on the WINNER. That keeps the
per-candidate cost what it was, at the price of R27's exactness property holding only approximately for rectangles — the
target used the centre, the contribution uses the drawn point, and the two differ by however much the cosine and the
distance vary across the face. Unbiased either way; the same standing the mixed-tint pool has had since the tools
started casting.

**The cast colour had to be stated, and that is a real exception to R30.** An emitter's tint is the mean of its texture,
which works for a torch because a torch's texture is mostly flame. Measured, all four ore textures mean out to stone —
saturation 0.08/0.09/0.18/0.09 against TorchSide's 0.74 — because an ore face is a wide grey field with a few flecks on
it. Weighting that mean by `emissive_ink`'s own gain moves it from 0.08 to 0.09; darkening the body cannot help, since
Aetherock was already at body 0.30 and no more coloured for it. **The problem is the texture's composition, not the
average taken over it**, and no derivation from a whole image recovers a hue its area does not carry. So the ore rows
state their cast colour, packed into LUT word 1 above the level — and it stays ONE authored thing, because
`BlockType::ore_flecks` is the same palette texgen paints the flecks and their spent siblings from.

**`GI_SURFACE_GAIN` is the calibration between two different quantities**, not a fudge factor. A lamp's level is an
intensity; a seam's is a radiance per unit area, and the area multiplies in. At unity a modest pocket lit a whole
chamber to daylight and the four tiers were indistinguishable, because everything sat near the top of the range and the
differences compressed away.

**What the captures showed.** `DIG_ORE=<tier>` is the scene and needed no work — it already hollows a chamber around the
nearest deposit of a tier and grants its level. Each tier reads as itself: ember orange, dusk violet, glowvein cyan,
aetherock pale gold, with the chamber dark beyond the pool at tier 1 and lit wall to wall at tier 4. The best frame is
tier 3's, and it was not planned: granting Aetherock's level reveals every tier below it, so a Glowvein seam and an
Emberore seam light the same chamber cyan and orange from opposite sides and blend across the floor with no hue noise —
the multi-emitter check R30 asked for, arriving on its own.

### R32 — an unproven answer must not be daylight

User report: a long tunnel underground — fifty blocks, dug at any depth, down to bedrock — shows its DISTANT blocks lit
by leaking daylight at noon while the near ones read correctly. The report is exact, and the near/far split is the
diagnosis: every leak path is a place the system ANSWERS sky on no data, and every one of those paths only engages at
range.

**The leak was several paths, not one.** At fifty blocks the cascade fallthrough has left C0's coverage and C1's lattice
probes are dead inside rock, so `gi_indirect` and `gi_cache_at` bottomed out in `gi_sky_fallback` — the live daytime
dome, visibility 1 — for every gather hit past the near field. Independently, a gather ray fired DOWN the tunnel axis
exhausts `GI_GATHER_T` without touching a wall, and `path_radiance` answered every miss with full sky radiance, proven
or not; the probes' own `march` did the same with a sun lobe on top. `gi_sun_vis` answered its lateral and iteration
exits "fully lit". And the participating medium in-scatters `sky_base` in proportion to view distance with no occlusion
term at all, underground exactly as in open air — the dominant, distance-proportional term of the report.

**The rule, stated once:** a march that PROVES open sky (the R18 ceiling test) keeps answering full daylight at any
depth — that is what keeps a shaft's floor lit from above — and an answer given on NO data fades with depth. One ramp,
`medium_sky_fade` in medium_common.wgsl (edges `SKY_FADE_FULL_Y`/`SKY_FADE_ZERO_Y`; the top edge equals `SEA_LEVEL`,
pinned by `the_sky_fade_tops_out_at_sea_level`), multiplies every unproven answer: `gi_sky_fallback` and its
compute-side twins in `gather_prev`/`cam_light`, the unproven exits of `gi_sun_vis` and both `march`es (the miss structs
now carry a `sky` flag and the exhaust cell, so proven and unproven are distinguishable at last), the medium's air and
down-welling in-scatter at the fragment's altitude, water's SSR-miss reflection fallback, and the water surface's
backdrop in-scatter. The resolve's exhausted gather ray now terminates in the probe cache at its exhaust point (the DDGI
ray-termination pattern, `dir` as the gather axis) mixed toward sky across the ramp band — so a sealed tunnel goes dark
while a tunnel MOUTH still propagates real light inward through live C0 probes.

**Why the guarantee is structural.** `smoothstep` saturates at its top edge, so at and above `SKY_FADE_FULL_Y` every
touched term is bit-identical to the R31 image — the surface world, the accepted lateral-exit statistic in "What is NOT
built", DIG_VAULT, DIG_ROOF and every BENCH.md baseline pose cannot move, and no retune elsewhere can re-open them.
Open-air terrain below `SEA_LEVEL` is essentially all underwater, where the extra darkening is the desired behavior: the
down-welling fade makes deep water darker with world depth on top of `WATER_SIGMA`'s own extinction, which was asked for
by name ("as a bonus the hack would make water naturally darker the deeper you go"). The one deliberately unfaded family
is anything PROVEN: sun rays and gather rays that clear the sky ceiling.

**Scene:** `DIG_TUNNEL=1` (new) seals a stone bore ~52 blocks long ahead of spawn — long enough that its far wall
photographs through every fallback regime at once, which no room-sized scene reaches. Pair with `--pos` at a low y; the
same run above `SKY_FADE_FULL_Y` is the geometry-identical control, since the fade saturates there.

**What the captures showed** (2026-08-16, seed 7, tod 90, fixed dt, 1200 frames, offscreen). The tunnel at
`--pos 100 6
100`: before, a visible daylight glow at the far end of a sealed unlit bore, its central band averaging RGB
(10.9, 6.5, 5.5)/255; after, uniform dark at (0.7, 0.7, 0.7) — the crosshair — with 95% of the frame's pixels moved. The
surface-guarantee triple at their standard poses — DIG_VAULT (`--pitch -20`), DIG_RIVER (`--pitch -35`), and the bare
spawn noon shot — each moved by mean 0.0006–0.0011 per channel on 0.2–0.3% of pixels, an order of magnitude under the
0.007 same-build repeat floor recorded in "What is NOT built": bit-identical up to the 1-spp gather's own noise, as the
saturating ramp promises.

### R33 — a player is lit by the same list as the wall behind them

User report: players aren't lit by nearby or held lights — underground, a player holding a torch is a black silhouette
with a glowing torch head, beside a wall the same torch lights perfectly well.

**Direct light has exactly one estimator, and players were reachable by none of it.** Since R27 the gather paths return
no emitter radiance at all; every photon a torch delivers arrives through NEE against the light list — the resolve's
per-pixel reservoir, or the probes' stochastic sample, whose result reaches a surface only as BOUNCE. players.wgsl ran a
probe gather and a sun ray and nothing else, and the resolve's half-res NEE can never cover a player: they are not in
the depth prepass, so the resolve texels under their silhouette belong to the terrain behind them. A body beside a torch
therefore received only the room's bounce — near black. (R28's framing of players as a consumer of the carried-light
list was true only via that bounce.)

**The fix is sharing, not duplicating.** The emitter list moved from the resolve's own bind group into gi_common's
group(2) — the resolve's pipeline layout already carried the gather group, so the move cost one binding — and
`resample_light`, `NeeLight`, `lcg_next` and the `LIGHT_*`/`GI_NEE_ITERS` calibration moved with it into gi_common.wgsl.
The resolve, water and players now read ONE declaration; players call the same `resample_light` plus one `gi_march`
shadow ray in their fragment shader, folded in beside the existing gather. Held items ride the same `fs`, so a remote's
torch lights their own hand and bucket. `resolve_and_probe_agree_on_light_calibration` now sweeps players too, and
`every_nee_consumer_runs_the_shared_estimator` pins that both fragment consumers define the function once and actually
call it.

**Two deliberate deviations from the resolve's call, both because players carry no accumulator and no à-trous.** The
seed is STATIC, hashed from sub-block world cells: the resolve's advances per frame because its temporal filter averages
the visibility Bernoulli out, and unfiltered that advance is raw flicker on a body — a static draw is a stable spatial
dither, and with one light in range the reservoir degenerates to the argmax deterministically. And past `gi_lateral_max`
the term simply drops (the resolve takes the cascade blend there); a player at that range covers a handful of pixels
under the medium's fade.

**The known geometric limit, named so nobody files it as a bug:** a player's OWN carried light hangs at
`gi::carried_origin`, on their capsule axis — INSIDE the body box — so every body face backfaces it and the NEE term is
zero for it by construction. Their body reads its own torch as the room's bounce, exactly as before; what R33 changes is
every OTHER source — placed torches, ore seams (the rectangle sampler comes along for free), and other players' carried
lights — which is the reported bug. `DIG_STUB=ginee` removes the players term together with the resolve's, so an
attribution run cannot measure half of one estimator.

**What the captures showed** (2026-08-16, seed 7, tod 90, fixed dt, 1200 frames, offscreen). The money shot is
`DIG_VAULT_DARK=1 DIG_SCREEN=torch DIG_PLAYERBOX=1` — a dark room whose only source is YOUR carried torch, with the fake
remote parked five blocks ahead: before, a black silhouette; after, a body and held block fully lit by the light in your
hand, 2.03% of the frame's pixels moved and all of them on the player. In `DIG_VAULT=1 DIG_PLAYERBOX=1` (floor torch
BEYOND the player) the fix reads more subtly and more correctly: the camera faces the player's shadowed side, and after
R33 that side sits at the same near-black as the wood cube's camera face beside it, with the torch's rim wrapping both
the same way — the player finally shades like the terrain next to them rather than like a hole in the scene.

### R34 — an unfiltered estimator may not gamble, and the confetti was never lit at all

User report, with a screenshot: since R33 players wear **black squares that move as the player moves** — hard-edged,
axis-aligned patches over the body and over the block in their hand, in a base lit by several torches.

R33's own note called its static seed "a stable spatial dither", and named the case where that is harmless: with ONE
light in range the reservoir degenerates to the argmax and there is nothing to dither. Both captures it shipped with had
exactly one light. What the reservoir actually carries is the WHOLE pool's unshadowed irradiance on ONE binary
visibility sample, which is the right trade in front of the resolve's accumulator — the average of that Bernoulli IS the
shadowed sum — and the wrong one in front of nothing. So each sub-block world cell of a body took either the entire
pool, or pure black if its one draw happened to be occluded; the seed is anchored to the WORLD and a body walks through
it, which is why the pattern crawled. The patch size is the seed's own quantization, and that is the tell: nothing else
in the player path is quantized to a fraction of a block.

**A consumer with no filter behind it has to ask every light it can afford.** `sum_lights` (gi_common.wgsl) takes the
brightest `GI_NEE_TAPS` candidates and marches a shadow ray at EACH, summing what survives. The pricing is unchanged and
now literally shared: `gi_light_score` — R33's target function, lifted out of `resample_light` — is what the reservoir
selects on and what this pays, so the two cannot drift about what a torch is worth. Selection is on the score and not on
list order, which nothing keeps stable (`flush_lights` selects, it does not sort). Where every emitter in range is
visible the taps ARE the pool, which is the room case, so a body lights like the wall behind it.

**What it costs, stated rather than hidden:** past `GI_NEE_TAPS` emitters in range a body is a little darker than the
terrain beside it, because the tail is dropped. That is a bias traded for a dither, deliberately — it holds still, it
shrinks as the tail does, and a capture can measure it. Measured on the seam-dense `DIG_ORE` chamber, where the light
list is long: the shipped `GI_NEE_TAPS` against four times as many moves the body by mean 0.0032 over the frame, 1.41%
of pixels and none of them off it. In a torch-lit room the tail is empty and the difference is nothing at all.

**What the captures showed** (2026-08-17, seed 7, fixed dt, offscreen; frame times in BENCH.md). Both R33 money shots
are **bit-identical** across the change (`DIG_VAULT_DARK=1 DIG_SCREEN=torch DIG_PLAYERBOX=1` and
`DIG_VAULT=1
DIG_PLAYERBOX=1`, worst channel 1/255) — one light in range, one answer, which is the calibration guarantee
stated as a measurement. The repro is `DIG_ORE=3 DIG_PLAYERBOX=torch DIG_SCREEN=torch --yaw 90`, a corridor lit by two
torches and a wall of Glowvein seams: before, the body is a checkerboard of warm cells and green cells — the whole pool
wearing whichever emitter's tint that cell drew — and after it is one smoothly lit body, 1.17% of pixels moved and all
of them on the player or its held item. Terrain is untouched by construction and measures it: a full-frame capture with
no player in it differs by less than the same-binary repeat of itself.

**The mixed-tint case is worth naming**, because it is what made the artifact so loud here: a reservoir draw carries the
pool at the tint of whichever emitter won, so a body between a torch and a seam dithered between warm and green, not
merely between lit and dark. A per-light sum has no such state to be wrong about — each tap arrives with its own tint.

**And then the same estimator went to the confetti**, which had the R33 bug and no dither because it had no estimator at
all. Level-up flakes were lit by the camera-light buffer — probe bounce plus a traced sun — which is spray's argument
and does not stretch to local lights: a burst is thrown where a player is STANDING, and the camera's distance to a torch
is not the flake's. Underground it showed exactly as R33 did before it: a cloud of BLACK paper hanging in a torch-lit
vault. So confetti.wgsl takes the gi_common prelude and the group(2) world bindings and calls `sum_lights` at each
flake's centre, in the VERTEX stage — the gather layout has allowed vertex-stage reads since water's per-vertex gather
(R17), a flake is a fraction of a block across so one answer serves its six corners, and a burst is a few hundred
vertices where a body is tens of thousands of pixels. The reach gate earns its keep here rather than being copied: a
burst is thrown over a REMOTE player's head too, and they can be most of the sync radius away.

**A flake has no normal**, which is the one thing the shared price had to learn. It tumbles, it is two-sided, and paper
that thin is lit from whichever side the light is on — so a receiver may pass the ZERO vector and be priced at
`GI_OMNI_COS` instead of a cosine. That is the mean of |cos| over a flat flake in a uniformly random orientation, so it
is the integral rather than a fudge, and it lives inside `gi_light_score` rather than in a second scoring function,
because a second function is a second price. Measured: in a torch-lit vault the burst goes from black to lit with a real
falloff across it, 1.34% of the frame moved and all of it paper; outdoors at noon, where nothing is in range, the
capture is **unchanged** — the term is zero by construction and the picture says so. The water pass that draws it does
not move.

**Spray and the viewmodel are deliberately still on the camera light**, and this is the honest edge of the change. A
waterfall is outdoor geometry lit by the sky, and the viewmodel carries its own rig a metre from the eye where the
camera's answer IS the item's; neither has shown the black-paper failure. If a lamp-lit indoor waterfall ever does, this
is the shape of its fix.

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

### R35 — glass lets the light through, and the bucket is the word for it

Blended glass landed (Glass, Crystal, the crystal slab, future panes): a third mesh bucket drawn in the water pass after
the water draw, absent from the depth prepass — so a pane shades like water does, probe gather per vertex plus the short
per-fragment sun DDA (R17's split), because the half-res resolve owns no texel of a surface the prepass never saw.

In the GI the see-through test is the BUCKET (`MeshBucket::Translucent`), never the render class — `Translucent`
-the-class still means water and its op-1 path-length counter everywhere. A tinted cell is LUT op **4**: the marches
multiply a per-class rgb filter into a throughput instead of stopping, and every consumer that applies `medium_water_tr`
multiplies `thru` beside it — all THREE NEE sites included (the probe's `nee`, the resolve's reservoir, and
`sum_lights`, which is what lights a body). That completeness is the whole reason the filter is a product and not a
medium: it has no path state, so no two rays can disagree about being inside glass. Two of the three shipped without it
briefly, and the symptom was a lamp behind a stained pane lighting a player white inside its own coloured pool. The
filter is DERIVED, not authored twice: the texture's mean alpha is the coverage and its alpha-weighted mean colour is
what the covered part passes (`material_lut`), packed in the LUT's first box word — free for every full-cube class,
since only synthetic shape classes carry boxes. A shaped translucent classifies as a FULL cell of its base id
(`gi_class` bypasses the synthetic classes), the accepted approximation: a box that attenuates instead of blocking has
no home in the box words.

The resolve's sun target has no colour lanes (its g and a carry the face code + history and the penumbra), so the
transmitted sun splits across the two outputs it does have: the sun lane takes the filter's LUMINANCE, and the rgb
indirect lane takes the CHROMATIC RESIDUAL — sun radiance x ndl x vis x strength x the water term x (thru − lum(thru)),
negative in the channels the filter absorbs — so the two lanes SUM to the full rgb transmission chunk.wgsl multiplies
out and a red pane's pool lands red, riding the indirect accumulator's own history and a-trous instead of a third MRT.

**The caster STRENGTH rides the residual explicitly**, and that is the load-bearing half of the split. The sun lane
leaves the resolve as a bare visibility fraction and the scene pass scales it by `light_color * light_dir.w`; the
residual leaves as a finished radiance that nothing scales downstream, so it must carry `light_dir.w` itself — which is
why the resolve's uniform block spends `sun_color.w` on it. Omit it and the halves sum to the transmission at full sun
only; worse, since the residual is NEGATIVE where the filter absorbs, any strength below `lum(T)` drives those channels
past zero and the pane starts SUBTRACTING light through the whole sunrise and sunset ramp.

The glass DRAW is transmissive, not alpha-blended, in two halves: a MULTIPLY pass scales the backdrop by each texel's
transmittance (per-face sqrt, so the face PAIR every closed volume presents costs one cell's T — the same T the marches
charge), then an ADDITIVE pass lays the covered fraction's lit surface and any ink on top. A pane over bright sky GLOWS
with what it passes — the thing coverage blending can never say.

**The halves do NOT commute across panes**, and that is worth stating plainly because the pass was built believing they
did. A pane is `dst*T + S`; composing two of those gives `T_A*S_B + S_A` one way and `T_B*S_A + S_B` the other, equal
only when the shines happen to balance. Running every tint and then every shine IS order-free, but only by letting each
shine escape every filter — stack panes and the transmission falls toward zero while the additive term grows without
bound, so a deep glass corridor gets BRIGHTER instead of darker. `GlassDraw::draw` therefore walks its chunks
back-to-front and runs a chunk's tint and shine together, which is exact whenever the panes sit in different chunks. Two
panes inside ONE chunk still share a tint: the standing approximation, and the reason winding still needs no split.

Three accepted v1 edges in the draw, all the same missing fact — the mesher never says whether a face has a PARTNER:

- A pane whose far face is culled by an opaque neighbour (glass laid on dirt, set into stone) shows the ray one face and
  charges `sqrt(T)` where its cell is worth `T`, so it reads too bright against its own backdrop.
- A wall `n` cells thick still presents one pair, so the draw charges `T` while the marches charge `T^n`. Thickness
  reads in the light and not in the glass.
- The shine half is per-FACE, so the pair convention has to be spent on the ink too (`INK_PER_FACE`) or a free-standing
  crystal glows twice as bright as one set into a wall. `GLASS_REFLECT` is exempt: it was tuned against the doubled term
  and already absorbs it.

All three want one vertex bit saying "this face has a partner", which nobody has spent yet.

A fourth edge sits on the MARCH side of the same ledger, and it is the one most likely to arrive as a BUG REPORT, so it
is written symptom first.

> **"Stained glass barely tints the block it is sitting on."** Also: a window's sill washed out while the floor just
> past it is properly coloured; a stained-glass FLOOR that colours the ground under it far less than the same glass
> colours a pool cast across the room. Suspect this edge before looking anywhere else.

**Cause: a tinted cell the shading point sits in is never charged.** Every march steps before it tests (the rule that
stopped shaped surfaces shadowing themselves), and `gi_ray_init` floors `p + dir*0.01`, so the first cell — the one the
lit face opens onto — is stepped out of before anything reads it. Glass touching a lit surface therefore filters that
surface's sunlight not at all. Light through a window several cells away is unaffected, because the pane is crossed
mid-march, which is why the feature's showcase looks right and this does not.

The tint is always WEAK, never absent, and the geometry is why: the artifact needs the origin cell to be glass, and if
it is, then seeing that face means looking through that same glass, so the draw still charges one face. A surface with a
pane laid on it comes out at `sqrt(T)` where it is worth `T^2` — the absorbed channels land several times too bright,
and it is a steady error with no shimmer or motion cue, which is what makes it easy to live with and hard to spot alone.
Side by side against a pool cast through a window it is obvious.

Of the two edges compounding here this is the DOMINANT one; the `sqrt(T)` pairing above is the smaller half, so a fix
that spends only one should spend this. The fix is a pre-charge before the loop at the three march sites, and it is not
free — one extra fetch per ray, on every ray, in every scene, to correct a case that needs glass touching the lit face.
Charging is safe there in a way testing is not (a tinted cell never sets `hit`, so it cannot re-introduce
self-shadowing), so this is a cost decision and not a correctness one.

Two deliberate conservatisms, recorded rather than discovered later. `gi_solid` (op >= 2) counts a tinted cell as solid,
which is what keeps probe RELOCATION and the probe-gather visibility test (`gi_occluded`) out of panes — no worse than
the Opaque rows these blocks were the day before. It does NOT make glass block anything else: every NEE shadow ray goes
through `march`/`gi_march`, which never set `hit` on op 4 and attenuate instead, which is the point. And the coarse
blocker bit is SET for tinted cells (`classify_chunk`): the coarse texture only licenses skipping a brick, and a skipped
brick accumulates no throughput, so a wall of panes would go untinted. The one-line fallback — glass as GI-air,
`opacity()`'s bucket arm answering `Air` — shipped as this feature's intermediate phase and lives in git if a bench ever
charges the marches more than the picture is worth.

### R36 — the light list took a cap, and the ray budgets became numbers a platform can set

Two levers landed together, both aimed at the mobile browser. The RAY budgets moved no default; the cap did —
`web_mobile()` now ships the Low lighting notch and the Light Sources floor (`LIGHT_CAP_MIN`), a start-cheap default a
phone player buys back up from, while every other profile keeps the full list. The motivating cost is scene-dependent:
`resample_light` scans the whole light list per resolve pixel per frame, so a torch field or a cave wall of revealed
veins is exactly where the frame gets expensive — and the platform that pays most is the one with the least headroom.

**The cap.** `Settings::max_lights` ("Light Sources") caps the flushed list at runtime. CPU-side ONLY: the buffer
allocation, the probe uniform's capacity word and the shader clamps stay sized to `MAX_LIGHTS` at compile time, and the
slider's own maximum is DERIVED from that capacity (`settings::LIGHT_CAP_MAX`), so a cap is never anything but a shorter
header count over the same allocation. `GiDraw` tracks the cap it last flushed under and dirties the list when it moves,
in either direction — without that, RAISING the cap would not re-admit the dropped lights until the camera next cleared
the refresh distance.

**Selection leans forward under pressure.** With more candidates than slots, nearest-to-the-camera kept a vein wall
BEHIND the player at the cost of the torches in front. The ranking key is now distance to the segment from the eye to
`SELECT_AHEAD` blocks along the look direction — the shader's `LIGHT_RANGE`, pinned by
`the_selection_lead_matches_the_shaders_light_range` — so under oversubscription the behind lights lose their slots
first. Three properties held on purpose: the eye stays on the key's zero set, so the carrier's own light still cannot be
the one dropped (`the_carriers_own_light_survives_any_cap`); the REACH filter stays camera-centred, because it mirrors
the marches' own lateral bound and probes behind the camera still need their lights — the bias reorders eviction, it
never culls; and an undersubscribed list is independent of the heading
(`an_oversubscribed_list_drops_the_lights_behind_first` sweeps both), so the common case pays only the key's arithmetic.
Staleness now compares the ANCHOR rather than the camera: a heading swing while oversubscribed moves the anchor exactly
as a walk does, so turning around refreshes the list on the same rule and the same distance, and a resting camera still
early-outs bit-exactly.

**The budgets.** The resolve's gather and sun iteration counts — the frame's two largest ray budgets, per BENCH.md's
attribution — moved from WGSL literals into the resolve uniform (`rz.budget`), with the shipped values now owned by
`gi/resolve.rs` (`GI_GATHER_ITERS`/`GI_SUN_ITERS`) and every profile shipping them unchanged. The marches' own loop
bounds stay literals in the shader as CAPACITIES — an over-budget value self-clamps in the loop, and
`budgets_fit_the_shaders_capacities` keeps the shipped budgets inside them. The levers are `DIG_GI_ITERS=<gather>,<sun>`
on desktop and `?giiters=` on web, and they are A/B levers, not settings: a shorter march silently un-shadows whatever
lies past the cut, so a default only moves on the measurement the "No tier scaling" bullet has always asked for — a
genuinely focused browser run — judged against the gi_scale_div image bar (BENCH.md 2026-08-12) and the 90-frame flicker
method. The decision rule is recorded here ahead of the numbers: within the bars at desktop resolution AND faster, the
default drops for everyone; within them only on mobile-web measurements, `web_mobile()` alone; outside them, the
defaults stay and the refutation goes to BENCH.md.

**The browser can finally measure itself.** `?stub=` gives the web launcher DIG_STUB's per-term attribution (same
parser, same fatal-typo rule), and `?torches=`/`?torchesfar=`/`?maxlights=` put the emitter-count bed and the cap on the
GPUs the mobile defaults are actually for — the far pad rides the courtyard plan, so `?torchesfar=` without `?torches=`
builds no scene. The pad's capacity grew from its R27 clamp to the light list's own, so the bed can oversubscribe
`MAX_LIGHTS`: the first 64 keep the exact cells R27 and R34 measured on, and the extension stacks VERTICALLY over a
compact footprint, because the reach filter is lateral and a pad growing away from the camera would fall out of reach at
precisely the short draw distances the bed exists to price.

### R37 — the temporal reservoir was built, measured working, and reverted anyway

R27 and R28 both end on the same deferred fix: visibility is the one thing `gi_light_score` cannot price, so wherever
emitters occlude each other the whole in-range pool rides one shadow ray's Bernoulli, and a temporal reservoir (ReSTIR
DI) was the assumed cure with 0.103 (`DIG_TORCHES_WALL`, the 90-frame walk diff) as the number to beat. R37 built it,
validated the estimator, and reverted the build on the measurement. This note is what remains, and it exists so the next
reader spends an afternoon here instead of the week.

**What was built.** A second fragment entry in the resolve module — its own pass, because the resolve's two accumulator
targets already spend the whole WebGPU default `maxColorAttachmentBytesPerSample` — holding the exact discrete RIS scan
plus the reservoir: ONE shadow ray on the FRESH winner, whose occlusion zeroes its merge weight while still counting in
M (that zero-yet-counted entry is the entire estimator — drop the "counts in M" half and a half-occluded wall converges
2× bright); a temporal candidate re-entering at its current score × stored weight × clamped M, reprojected through
`prev_vp` and validated against the resolve's own history attachments at the GRAZE-widened tolerance (the strict one
starves the reservoir on walking frames, the same population the resolve's relaxed pass exists for); identity stored as
position + packed w word and re-matched inside the scan, because `flush_lights` selects and never sorts, so an index
means nothing across frames; and trusted visibility for the un-marched winner — safe in this engine alone, because
occluders are voxels and every voxel change carries an inval sphere, which restarts the reservoir. A Monte Carlo
reference implementation preceded the WGSL: unbiased on every occlusion pattern tried, EXACT on an all-visible pool, ~9×
less flicker in the sim. One trap worth keeping from the wreckage: `fs`'s `uv` is scaled for the RENDER-size depth copy,
so a GI-sized input read at `uv` lands its content a factor of `gi_scale_div` up-left — a new same-size input must be
read at `i.pos.xy`.

**The estimator works.** Night, 8 torches, walking, the 90-frame same-pose diff, on the real build: the raw NEE term's
own flicker fell 0.646 → 0.159 with the wall screened — the 4× the sim promised, before any filter.

**The image does not move, and the open case regresses.** Same protocol on the finished frame: with the temporal half
force-disabled the restructure is bit-identical to the shipped build (wall 0.0944, open 0.0675 — the baseline
reproduced); enabled, the wall reads 0.0996 and the open 0.0774. The wall's excess over the open case — the exact number
the reservoir was built to close — did not shrink, and the open case, where the estimator is provably exact, picked up
15%. Full table in BENCH.md 2026-08-21.

**Why, and why it is structural rather than a tuning miss.** The NEE term is folded into `indirect` and then filtered
twice — the screen accumulator (`N_MOVING` deep on exactly the walking frames that flicker) and the a-trous pass. A
temporal reservoir is itself a temporal filter whose window is the M-clamp, and that window is SHORTER than the
accumulator's. Reuse does not remove variance, it moves it down the spectrum: the memoryless Bernoulli is white,
per-pixel independent noise — precisely what the accumulator and a spatial filter suppress best — while the reservoir's
slowly-relaxing weight rides through a filter whose gain assumes independent samples. The reservoir traded noise the
pipeline already removes for wander it cannot, which is why 4× off the raw term bought slightly WORSE on the finished
frame. No M-clamp fixes this: shorter clamps converge back to the memoryless estimator, longer ones wander slower still.
ReSTIR earns its keep where the reservoir's output is shaded with little or no history behind it; this engine's direct
term does not qualify, and that is a property of the architecture, not of the tuning.

**What would reopen it.** Removing or drastically shortening the screen accumulator, or a consumer that needs
low-variance direct light with no filter behind it — and the unfiltered consumers this engine has (players, confetti)
already run `sum_lights` instead, deterministically, for exactly that reason (R34).

## Non-goals (v1)

- Specular GI / reflections (SSR stays as-is for water).
- Shadows beyond the voxel window (analytic fallback, mostly under fog; since R32 the fallback fades with depth below
  `SKY_FADE_FULL_Y`, so "assume lit" is confined to the surface world where it is nearly always right).
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
into synthetic PARTIAL classes from `SHAPE_CLASS_BASE` up, each carrying a two-box cover of the shape's LIGHT boxes
(`shapes::light_boxes` — its collision boxes, except that a fixture a body walks through, the torch, still stands in the
light's way; box 0 = the first box exactly, box 1 = the AABB union of the rest — over-blocks only, never under, worst
case the old full cell), and a state with no light boxes at all (an open door) classifies as air, which is what lets
light through a doorway. A chunk re-uploads as one `write_texture`; scrolling uploads only entering slabs.

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
instead of a firefly. Since R33 it rides gi_common's shared group(2) rather than the resolve's own group, because the
resolve and players run the one `resample_light` over it and must read one declaration.

**Probe cascades** — two toroidal camera-following grids of 6-axis irradiance probes: C0 fine and near, C1 several times
coarser and correspondingly wider, each a fixed grid whose dimensions (and iOS's smaller pair) live in `gi.rs`; coverage
is spacing × grid size. Payload: one rgba16float 3D atlas per cascade, dims (nx, ny, nz×6) — axis-major Z slabs,
hardware trilinear with a half-texel Z inset so slabs never bleed. Ping-ponged: the update compute reads last frame's
atlas (that read IS the infinite bounce) and writes the new one; a full copy precedes the amortized write. A r32uint
flags texture per cascade carries, in order, the probe's age, a dead-in-solid bit and a relocation code (the probe
centre nudged into air, at most a block). Beyond C1: analytic fallback (ambient dome × face weight + lambert sun,
visibility 1), faded in over C1's outermost band — and, since R32, depth-faded through `medium_sky_fade`, because that
fallback is an unproven answer and underground it was the daylight leaking into every long tunnel.

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
sweep, iOS the coarsest resolve. The full-resolution divisor is a capture lever (`settings::GI_SCALE_DIV_FULL`), never a
settings rung: behind the à-trous filter and the bilateral upsample it buys almost no visible picture for the largest
cost step the slider could take (BENCH.md 2026-08-12), and its accumulators at retina fullscreen are large enough to
destabilize a browser's GPU process.

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

The ones that bite if broken. CLAUDE.md points here rather than restating them.

**Buffers and history**

- The voxel window is DERIVED data: never serialized, never on the wire; device loss rebuilds it by re-upload + age
  reset (drill-tested). Classification is a pure function of the cell value, pinned headless.
- Everything the rays read (window, LUT, coarse, light list, atlases) lives on the GPU; the CPU never reads probes back.
- **The accumulator must be rgba32f** (`gi::GI_ACC_FORMAT`). A half-float's mantissa is too short to resolve the
  smallest update a converged history can make, so it rounds to zero and every pixel freezes where it stood
  (`history_format_resolves_the_accumulator`).
- **The two lighting pairs have FIXED roles, not ping-pong**: `[0] ←resolve→ atrous(1) → [1] → atrous(2) → [0] →scene`.
  That leaves the once-filtered image parked in `[1]` as next frame's history, which is what SVGF feeds back.
- **History length `n` rides the sun target's g channel**, packed above the face code (`packed_metadata_stays_exact`),
  because WebGPU's per-attachment byte budget is spent. Every reader masks the code back out.
- **The history fetch is bilinear, per-tap validated on face code and view distance, and `n` is two-state** — snapped
  onto the lattice, or `N_MOVING`. Travel is not zero at rest (R13). `a_moving_pixel_always_gets_the_spatial_filter`
  pins `N_MOVING < N_CONVERGED`.
- **No moments buffer.** Both estimators have an analytic variance from data already stored, which saves a ping-ponged
  rgba32f and cannot drift out of step with the accumulator.

**Transport and thresholds**

- **Sun thresholds are in RADIANCE** (`SUN_MIN_L`, weight × `light_color`), never a bare blend weight. There is no
  tonemap, so a weight that looks negligible against an HDR sky is a large error in the light it admits (R21).
- **Twilight strength (`light_dir.w`) multiplies the LIGHT, never the shadow.**
  `mix(vec3f(1.0), visibility,
  light_dir.w)` is the shadow-map-era form; against a traced visibility it returns 1.0 as
  strength falls, lighting every caster-facing surface at twilight through walls.
- **Water writes OPAQUE** (R25). The fragment samples `scene_tex` and runs the whole transport across `thick`, so
  `back + surf` already IS the backdrop. Alpha is the falling-sheet streak gate alone.
- **`medium_common.wgsl` is the one participating medium**, with L_in = `sky_base`, the same function the sky pass
  draws, so terrain fades into exactly the sky pixel behind it and the streaming rim disappears. It is the ONLY water
  extinction — `gi_sun_vis` returns a water PATH LENGTH so the sun ray and the view ray cannot disagree. There is no
  fog, no INDIRECT_K, no tonemap.
- **Unproven daylight fades with depth** (R32). A march that PROVES open sky answers full daylight at any depth; every
  answer given on no data multiplies by `medium_sky_fade`, whose top edge equals `SEA_LEVEL` and saturates there — so
  the surface world is bit-identical by construction and only underground goes honestly dark.

**The DDA**

- **`gi_dda.wgsl` is the one DDA**, dependency-free so the compute kernels can share it. It owns the two-level brick
  skip, the sky ceiling test (R18), `gi_lateral_max` and `gi_boxes_hit` (R16). Every partial cell blocks only where its
  boxes stand, emitters included. **Every march steps BEFORE it tests**, since a shaped block's surface lies inside its
  own opaque cell.
- **Per-block tint multiplies albedo, never light** (R23). The à-trous filter and the accumulator only see the lighting
  buffers, so a per-block step in the light is a signal they are built to smooth away. Structurally excluded from ink.

**Emitters**

- Light is three independent block properties: `emissive` (the `glows` builder — fullbright ink), `light`
  (`emits(level)` — the cast level), and `surface_light` (`seams(level)`, R31) which radiates from exposed FACES rather
  than the cell centre. `gi::classify_chunk` places no point source for a `seams` block; the MESHER hands the renderer
  merged rectangles instead, because it is the one pass holding the ore reveal, face exposure and the neighbour shell at
  once. **Every node row is `seams` and nothing else is** — buried rock must light nothing, and a point light one course
  behind a wall leaks through it.
- `LIGHT_RANGE`/`LIGHT_GAIN`/`LIGHT_NEAR` are pinned across both shaders by
  `resolve_and_probe_agree_on_light_calibration`. Both gains are stopgaps for the missing tonemap, which cannot simply
  be appended — the offscreen scene colour uses the SURFACE format, so radiance is already clamped at the scene pass.
- **An ink texture is authored for fullbright, not for lit** (R29). A pale one has no headroom left to take
  `emissive_ink`'s gain and clips on all three channels.
- NEE's stop-short margin is NOT a constant: `gi_light_margin` is the light's distance to the farthest corner of its own
  cell (R26), asked through `gi_light_margin_of`, which answers **zero** for a light with no cell of its own
  (`GI_LIGHT_CARRIED` — a carried light's origin is on its carrier's capsule axis, so a wall half a block away is meant
  to block it) and only `GI_LIGHT_SLACK` for a RECTANGLE, whose sample point already sits on the rock face.
- **Direct light has exactly one PRICE**, and the light list lives on the shared gather group so nothing can quote a
  different one: the gather returns no emitter radiance at all, and `gi_light_score` is what every NEE selects on. What
  differs is how much of the pool one fragment dares put on a single ray — the resolve resamples through a one-pass
  reservoir (R27), players run `sum_lights` because **nothing filters a body** (R33, R34), and confetti prices per flake
  in the VERTEX stage with the ZERO normal `gi_light_score` reads as isotropic.
- **A carried light is an ordinary entry in that same list** (R28) — no second lighting path, which is what gives it
  bounce, hard shadows, water response and a lit hand for free. It hangs at `gi::carried_origin(eye)`, a function of
  POSITION and not facing, so mouse-look cannot move it and a player standing still is not a moving emitter. Motion is
  expressed as re-placement, and the trigger is a DISTANCE, never a cell crossing (which on a boundary fires every frame
  for a player going nowhere). **Putting a light OUT re-places it too**, which covers a swap, a disconnect and a remote
  leaving the snapshot.
- `emissive_ink` in `ink_common.wgsl` is the one ink function, prepended to chunk/viewmodel/players so a `glows` block
  reads the same in the world, in your hand and in somebody else's (`every_pass_that_draws_ink_shares_one_gain`).
- **The see-through predicate is the mesh BUCKET** (R35), swept by `the_bucket_is_the_see_through_predicate`:
  `MeshBucket::Translucent` is op 4 in the LUT, and `Translucent`-the-class still means water everywhere. Glass
  throughput is a FILTER PRODUCT, not a medium — it has no path state, so the sun and view rays cannot disagree about
  being inside it, and water remains the one participating medium. That only holds while EVERY consumer multiplies
  `thru`: all three NEE sites, not just the probe's. A tinted cell pins its coarse brick
  (`coarse_bits_split_blockers_from_emitters`), or a homogeneous run of panes would be skipped untinted.
- **The bucket is the see-through predicate OUTSIDE lighting too**, and each site that forgot it grew its own hole: the
  water arm of `face_visible`, the shaped emitter's cull, and the water surface's cover test all key on it now, swept by
  `a_translucent_cell_never_covers_whatever_its_shape`. A render CLASS cannot answer this — a crystal slab is `Shaped`.
- **The glass draw's two halves do not commute** (R35), so the chunk list is walked BACK TO FRONT with both halves run
  per chunk. Running all tints then all shines lets every shine escape every filter, which diverges as panes stack.
- **The light cap is CPU-side only** (R36): `Settings::max_lights` shortens the flushed header count, while the buffer,
  the probe uniform and the shader clamps stay sized to `MAX_LIGHTS` at compile time — the slider maximum is DERIVED
  from that capacity. Under oversubscription selection ranks by distance to the eye→`SELECT_AHEAD` segment (the shader's
  `LIGHT_RANGE`, pinned); the eye stays on the key's zero set so the carrier's own light is never the one dropped, and
  the REACH filter stays camera-centred — the forward bias reorders eviction, it never culls a light the probes behind
  the camera still need.

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
  went with them, their design doc, the flag itself).
