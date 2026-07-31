# Bench baseline & hotspot analysis

All numbers: Apple Silicon (12-core: 8P+4E), release build, `just bench`
(seed 42, tod 45, 1800 frames, scripted flight, inputs off), ×3 runs.

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
