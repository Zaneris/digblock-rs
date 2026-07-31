# Bench baseline & hotspot analysis

Baseline: `just bench` ×3 on 2026-07-31, Apple Silicon, release build,
1800 frames (~15s), seed 42, tod 45. Kept as `bench-baseline.csv`.

## Run-to-run variance (noise floor)

| run | avg ms | p95 | p99 | worst | fps |
|-----|--------|-----|-----|-------|-----|
| 1 | 8.33 | 10.63 | 11.57 | 12.26 | 118.8 |
| 2 | 8.38 | 10.84 | 12.16 | 18.05 | 118.5 |
| 3 | 8.35 | 10.64 | 11.71 | 14.18 | 118.6 |

avg_ms spread **0.6%**; per-phase columns repeat within ~1-3%. `worst`
is noisy (single-frame outliers) — don't gate on it. One startup hitch
per run, consistently.

## Where the time actually goes (steady state, per frame)

The display is 120Hz Fifo: total 8.33ms is vsync-locked, and
`acq` ≈ 6.8-7.2ms of it is idle swapchain wait. Real work:

| phase | ms | notes |
|-------|-----|-------|
| enc (CPU encode+submit) | 0.79-1.24 | largest CPU cost; iterates all ~2.4k resident chunks in the main cull loop **and** each shadow pass, plus a fresh `Vec` alloc + sort for water every frame |
| gpu shadow | 0.61-1.12 | largest GPU pass; spikes when the wide cascade refreshes |
| gpu main | 0.55-0.75 | |
| upl (evict+upload) | 0.16-0.22 | small, **but** `sat` fires ~580 of ~600 frames per window — UPLOAD_BUDGET=4 is saturated essentially every streaming frame |
| mnt (pipeline maintain) | 0.05 | negligible |
| upd (game update) | 0.00 | negligible |
| gen job (worker) | 0.03 avg / ≤0.5 max | negligible |
| mesh job (worker) | 0.07 avg / ≤1.7 max | cheap; but 200-540 results per window are **stale-dropped** (~15% of mesh work wasted to remesh cascades) |

## Ranked findings → M9 scope

1. **UPLOAD_BUDGET=4 saturates ~97% of streaming frames** while the
   upload phase costs only ~0.2ms. Raising the budget (or making it
   adaptive on queue pressure) clears the mesh backlog dramatically
   faster with essentially zero frame-time risk. Success metric: `sat`
   collapses, world fills sooner, `worst_ms`/`p99` unchanged.
2. **Encode cost (~1.2ms)**: hoist the per-frame water `Vec`
   alloc+sort to a reused buffer; avoid re-deriving chunk centers per
   pass. Success metric: `enc` down measurably (noise floor ~0.05ms).
   Matters most for mobile (fewer cores) and future draw-distance
   growth — desktop is vsync-bound with headroom.
3. **Skipped (data says negligible)**: gen-job column caching
   (0.03ms avg), GPU buffer pooling (upload is 0.2ms), stale-drop
   reduction (500 × 0.07ms ≈ 35ms of *worker* time per 5s across 4
   threads — invisible).

Note for future A/B runs: overall fps/avg_ms is vsync-capped at 8.33ms —
judge optimizations by their **phase column**, not total frame time.

## Applied optimizations & results (same day, ×3 runs each)

**1. Adaptive upload budget** (`engine.rs`): budget doubles under
saturation (4→…→32 cap), decays by 1 back to 4 when clear.

| metric (per 5s window) | before | after |
|------------------------|--------|-------|
| `sat` (budget-saturated frames) | 578-601 (~97%) | 50-67 (~10%) |
| uploads | ~2.3-2.9k | ~3.6-5.1k |
| stale-dropped meshes | 172-539 | 107-197 |
| startup hitch | 1 every run | **0 every run** |
| `upl` ms | 0.16-0.22 | 0.24-0.35 |
| p99 ms | 11.6-12.2 | 12.1-12.4 |
| avg ms | 8.33-8.38 | 8.33-8.37 |

World fills roughly twice as fast; frame-time cost within noise; the cap
keeps the worst frame bounded (worst unchanged).

**2. Water-sort scratch reuse** (`renderer.rs`): the per-frame
`Vec` alloc + sort now reuses a persistent buffer keyed by `ChunkPos`.
Effect is below the `enc` noise floor at ~200 water chunks — kept
because it's strictly less work, but the measurable `enc` lever is
draw-call count (batching/instancing), which is future work, not a
tweak.

**Skipped after measurement**: gen-job caching, GPU buffer pooling,
stale-drop reduction — all measured negligible (see ranking above).
