# Bench baseline & hotspot analysis

All numbers: Apple Silicon (12-core: 8P+4E), release build, `just bench`
(seed 42, tod 45, 1800 frames, scripted flight, inputs off), ×3 runs.

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
