# Stage-B voxel light

Status: **P1–P3 built (field, torchlight, skylight); P4 pending.** This doc is
written first, per SKILLS-DESIGN §7.4's reservation ("Stage B: its own design
doc, later"). Phases land separately, each behind the full gate (`just check` +
`just check-ios` + `just check-web`). Capture helpers: night `DIG_SHAPES=1`
photographs torch pools; `DIG_VAULT=1` seals a stone vault around the spawn
camera with one torch inside — the flagship "roofed room goes dark at noon"
shot.

## 1. Goals / non-goals

A torch must **illuminate its surroundings** with correct occlusion: no light
through a wall, no light behind a wall, no lit cell the light could not have
reached by an open path. Light is a **client-side derived quantity** — a pure
function of the resident blocks — and therefore:

- never serialized: not on the wire, not in the journal, not in saves. The cell
  u16's spare bits 10–11 travel both formats and are the wrong home.
- never on the server: the server has no mesher and nothing reads light there.
- never persisted across device loss or eviction: the field is CPU-side, so
  device loss re-uploads meshes built from the *existing* field (no relight);
  an evicted chunk's light is dropped and recomputed on re-entry.

Non-goals (deliberately): colored light (one channel per source class, not per
hue), dynamic/moving lights (block cells only), and light-reactive gameplay
(mob spawning etc. — nothing reads the field but the mesher).

## 2. Data model

`crates/digblock-core/src/light.rs`:

```rust
pub enum LightChunk { Uniform(u8), Mixed(Box<[u8; CHUNK_VOLUME]>) }
pub struct LightField { chunks, add, remove, dirty }   // sibling map on Pipeline
```

One byte per cell: **low nibble blocklight** (P1), **high nibble skylight**
(P3). A missing map entry reads as `Uniform(0)`; `Mixed` allocates on the first
divergent write. Sky chunks become `Uniform(0xF0)` in P3 without allocating.

The field lives **beside** the chunk store on `Pipeline` (`FxHashMap<ChunkPos,
LightChunk>`), not inside `ChunkSlot`: the BFS reads blocks while writing
light (disjoint-field borrows), and shared `AIR_CHUNK` sentinel chunks still
carry per-cell light (a torch on a hilltop lights the air above it).

## 3. Propagation

Classic two-queue BFS. **Opacity is exactly the mesher's AO rule**
(`RenderClass::Opaque` blocks light; Cutout/Translucent/Shaped pass — what you
can see through, light passes through). Occlusion correctness is structural:
light only reaches a cell by stepping through open cells.

- **Add**: pop a position, re-read its current level (stale entries
  self-discard), spread `level − 1` into each of the 6 neighbors that is
  non-opaque and dimmer than that.
- **Remove**: pop `(position, old_level)`; each lit neighbor either had less
  (`< old`: zero it, push remove — and if it is itself an emitter, re-seed its
  own emission and push add) or is independently supported (`≥ old`: push add —
  the relight frontier).
- **Edit hook** (`on_cell_changed`, called from `Pipeline::set_cell`, which is
  the single local-write funnel): (1) if the cell's stored light > new
  emission, zero and push remove at the old value; (2) if the new cell emits,
  write the emission and push add; (3) if the new cell is non-opaque, push its
  6 neighbors to add so surrounding light floods in (a dug wall). One uniform
  rule covers torch place, torch dig, wall place, wall dig, and fluid writes.
- **Arrival seeding** (`drain_results`, after the overlay re-applies): scan the
  landed column's chunks for emitting cells (push add), then scan the four
  horizontal neighbor columns' facing light-border layers for cells ≥ 2 (push
  add) so an existing field spreads into the newcomer — the light mirror of
  the `missing[dir]` re-dirty rule. Deferred/authoritative edits already route
  through `set_cell` and need nothing extra.
- **Unloaded cells are walls**: the BFS never writes into or reads through a
  non-resident chunk; arrival seeding is what repairs the boundary later.

### Ordering: settle-before-dispatch

`step_light(LIGHT_BUDGET)` runs at two sites in `Pipeline::maintain`: before
`run_inline_jobs`, and between `drain_results` and `top_up_workers`. Mesh
snapshots are built at *dispatch* (`next_mesh_job`), so every snapshot sees
settled light — a mesh is never built from a half-propagated field and then
rebuilt. Removes drain before adds within a step. `LIGHT_BUDGET` (16 384 pops
per maintain) settles a full torch (~5k cells) in one frame; only mass
streaming carries over, and dirty-gating makes carry-over merely late, never
wrong.

### Remesh gating

Every light write records its chunk **plus the neighbors whose padded shell
contains the cell** (the same `offsets` rule as `set_cell`) into a dirty set.
After each `step_light`, `Pipeline` drains the set and bumps `mesh_gen` for
Ready non-air chunks, reusing the `empty_mesh_gen` skip (light shades faces,
never creates them — an empty mesh stays empty) and the `submitted_gen` skip
(a queued job snapshots at dispatch and already sees the settled field).
Chunks with no light delta never remesh. Worst case is the 3×3×3 neighborhood
(level 15 reaches ≤ 15 < 16 blocks); typical torch edits dirty ~8 chunks.

## 4. Sources

`BlockProps.emissive` (Stage-A fullbright ink) stays a bool and keeps its
meaning. Stage B adds `BlockProps.light: u8` (0–15) via the `emits(p, level)`
builder — Torch = 14; SKILLS-DESIGN's Lantern (14) and Aetherlamp (15) slot in
as rows. The two are independent on purpose: M5's ore-reveal seams want ink
without casting.

## 5. Vertex format and shading (P2/P3/P4)

- Solid word 1: blocklight bits 22–25, skylight 26–29 (30–31 stay free).
  Water word 1: blocklight 24–27, skylight 28–31. Per-corner values via
  `face_light` (the `face_ao` walk, averaging the non-opaque corner cells —
  smooth light); shaped blocks bilinearly sample the parent cube's face values
  exactly like parent-corner AO. Triangulation flip stays keyed on AO alone.
- `chunk.wgsl`: `col = tex * ao * max(sun_term, TORCH_COLOR * curve(bl))` —
  `max`, not `+`, so noon faces don't blow out beside torches; the Stage-A
  emissive early-return and `apply_fog` are unchanged.
- P3 scales `ambient`/`direct` by skylight with a `MIN_AMBIENT` floor: this is
  what makes caves dark and torchlight readable. Skylight is the real column
  channel (descends undecremented through non-opaque cells, spreads sideways
  with decrement), not a depth heuristic — a player-roofed room must go dark.
- `water.wgsl` carries light in the widened `world.w` varying (the pass sits
  at 14 of 16 inter-stage variables); folded into scatter and froth, never the
  specular. `shadow.wgsl` reads word 0 only and never changes.

## 6. Budgets and metrics

`light_pops` (BFS pops this maintain) and the pending queue depth ride
`Maintenance` → the 5s metrics line, CSV, and overlay, so a propagation storm
is visible in the same place streaming spikes are. The budget is a count, not
a deadline — pops are uniform-cost, and determinism (`any_step_budget_settles
_to_the_same_field`) is a pinned property.

## 7. Test pins

Core: sealed-box golden field around a torch; no light through an opaque wall;
around-a-corner pays the detour; removing one of two torches leaves the
other's field; placing a wall casts a correct shadow (remove-then-relight);
cross-chunk equals the unsplit reference; late neighbor is seeded on arrival;
any budget settles to the same field; only shell-touched chunks get remesh
bumps; eviction drops light and re-entry relights. Mesher (P2): vertex
round-trip with the carved bits; corner averaging ignores opaque cells; shaped
vertices interpolate the parent face. Skylight (P3): open column full to the
ground; a roof zeroes beneath and digging it floods the shaft; a cave mouth
attenuates sideways. Captures: night `DIG_SHAPES` (P2), cave interior (P3),
`just run --drill` each phase.

## 8. Phasing

| phase | lands | visible change |
|---|---|---|
| P1 | field, hooks, seeding, budget, metrics, core pins | none |
| P2 | mesher sampling, vertex bits, chunk.wgsl | torches light surfaces at night |
| P3 | skylight channel, ambient scaling | dark caves, dark roofed rooms |
| P4 | water shading, tuning, bench | lit water, final look |
