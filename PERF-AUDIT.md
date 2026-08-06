# Hot-path performance audit — frame-timing consistency (web-focused)

> **Status: implemented.** Everything below has been applied except the
> column-gen job slicing in §1, which measurement made much less urgent
> than this document projected — see "Implementation notes" at the end.
> Results and the new web baseline are in `BENCH.md`; this file is kept
> as the reasoning record for why each change was made.

Date: 2026-08-06. Scope: every per-frame path in the workspace, audited for
*consistency* (spike elimination) rather than throughput, with the wasm
build treated as the sensitive target (single thread, chunk jobs inline,
rAF-paced).

Companion to `BENCH.md`, which holds the desktop throughput baselines. The
short version of how these differ: `BENCH.md`'s remaining `over-12.5ms`
counts were traced to compositor noise on the dev machine, so the desktop
render loop is close to its environmental floor. The web build has never
had equivalent instrumentation, and that is where every finding below
concentrates.

Severity is judged by *effect on the worst frame*, not average cost. A
0.1 ms/frame steady tax ranks below a 12 ms once-a-second hitch.

---

## 0. Headline: the adaptive stream budget is inert on web

**Verified by direct execution against the shipped controller.** This is
the most important finding in the audit — the feature landed in `f28d7b1`
and tuned in `11be34c` cannot grow above its floor on the platform it was
written for.

### Mechanism

`StreamBudget::on_frame` derives busy time as `total_ms − acquire_ms`
(`crates/digblock-core/src/stream_budget.rs:106`), fed from
`engine.rs:1341-1345`:

- `total_ms` is the **frame-to-frame wall delta** (`metrics.rs:295`), i.e.
  it includes whatever the platform spent idle between frames.
- `acquire_ms` is time inside `surface.get_current_texture()`
  (`platform/src/app.rs:565-616`).

On native FIFO that subtraction is correct: the vsync wait blocks inside
the swapchain acquire, so `total − acquire` isolates real work.

On web it is not. WebGPU's `GPUCanvasContext.getCurrentTexture()` is
synchronous and non-blocking by specification — it *cannot* block, because
that would deadlock the JS main thread. Pacing instead comes from
`requestAnimationFrame` scheduling *between* frames (winit drives redraws
through rAF; `app.rs:118` parks in `ControlFlow::Wait` and every frame ends
with `request_redraw()`). So on web `acquire_ms ≈ 0` and `total_ms` ≈ the
full display interval, idle included.

Result: busy always reads ≈ the refresh interval, so
`headroom = 16.7 − 2.0 − 16.7 < 0`, the full-deficit backoff at
`stream_budget.rs:110-113` fires every frame, and the budget is pinned at
the floor permanently.

### Evidence

Running the real `StreamBudget` (floor 3 ms, ceiling 10 ms, 600 frames,
urgency 1.0):

```
--- NATIVE FIFO shape (vsync wait inside acquire) ---
120Hz native, 2ms real work      total= 8.3 acq= 6.3  -> final=10.00ms
60Hz  native, 5ms real work      total=16.7 acq=11.7  -> final=10.00ms
60Hz  native,10ms real work      total=16.7 acq= 6.7  -> final=10.00ms

--- WEB rAF shape (acquire never blocks; idle folded into total) ---
60Hz web, 2ms real work          total=16.7 acq= 0.0  -> final= 3.00ms
60Hz web, 5ms real work          total=16.7 acq= 0.0  -> final= 3.00ms
60Hz web, 8ms real work          total=16.7 acq= 0.0  -> final= 3.00ms

--- WEB on a 120Hz display (rAF quantizes total to 8.3 / 16.7) ---
120Hz web, work fits one slot    total= 8.3 acq= 0.0  -> final=10.00ms
120Hz web, work spills to 2 slots total=16.7 acq= 0.0 -> final= 3.00ms

--- with a TRUE busy signal (busy fed directly) ---
web, true busy  2ms  -> 10.00ms      web, true busy 13.0ms -> 10.00ms
web, true busy  8ms  -> 10.00ms      web, true busy 15.5ms ->  3.00ms
```

At 60 Hz the budget never leaves the floor even with 2 ms of real work per
frame. At 120 Hz it is bistable — full ceiling while work fits one 8.3 ms
slot, hard collapse to floor the moment it spills into two — which
produces a ~0.5–1 s sawtooth between 120 fps and 60 fps pacing during
near-field streaming, because the signal is quantized and can never
observe a busy value *between* 8.3 and 16.7.

The existing unit tests encode this without naming it:
`near_miss_collapses_to_floor` (`stream_budget.rs:187`) feeds
`on_frame(16.4, 0.4, 1.0)` — precisely the web steady-state sample — and
asserts collapse to the floor.

### Fix

Measure busy directly instead of inferring it. Two options:

1. **Preferred**: timestamp at rAF-callback entry through to just after
   `queue.submit()`, and feed that as busy. This is what the browser
   itself considers your main-thread occupancy.
2. **Zero-new-plumbing**: sum the phases already measured —
   `update_ms + maintain_ms + upload_ms + encode_ms` — and pass that as
   busy with `acquire = 0`. Less accurate (misses the unattributed gap in
   §5.M8) but immediately correct in sign and magnitude.

Then add a test with the web signal shape so the regression can't return.
Note that fixing this *raises* streaming aggression on web, so land it
together with §1 and §2 or the newly-granted budget will be spent on
oversized jobs.

---

## 1. The uninterruptible column-gen job sets the web spike floor

**`pipeline.rs:36-44, 164-170, 665-703`, `worldgen/mod.rs:151-183`**

`Job::Generate` builds an entire 16-chunk vertical column — heightmap,
fill, cave carve, decorate — as one indivisible unit. The inline executor
runs one unconditionally every frame that has gen work pending (the
spawn-fall-through guarantee from `d4a8859`), *regardless of budget*.

Measured natively: `build_column` p50 0.21 ms on ocean, **max 2.9 ms** on
mountains — bimodal, exactly as the `CostRing::new(12.0)` seed
(`pipeline.rs:223`) already anticipates. The 12 ms seed is the honest wasm
estimate, and 12 ms exceeds a 60 Hz frame outright.

So: **no budget, controller, or urgency scaling can reduce the worst web
frame during fill, teleport, or draw-distance growth.** The floor batch
alone can blow the interval. This is the structural spike.

Fixes, in increasing order of invasiveness:

- **Shrink the job** (see §2 — cave noise is ~80% of it).
- **Slice the job into continuations.** Split `Generate` into
  `column_shapes` (the 18×18 field grid + structure apron) and per-chunk
  `fill + decorate` slices that consume the shared shape state. The walk
  gate from `d4a8859` already tolerates multi-frame column landing. This
  is the same move as Naughty Dog's fiber job system (Gyrling, GDC 2015) —
  long jobs must be able to yield — implemented as explicit state-machine
  continuations since wasm has no threads.
- **Admission control on the floor batch.** The guarantee only requires
  the *player's own column*; every other gen can be deferred when its
  predicted cost exceeds remaining frame headroom.

## 2. Worldgen: cave noise dominates land-column cost

**`worldgen/mod.rs:125-131` → `caves.rs:28-41`**

`caves::carved` runs 1–2 `noise3` evaluations for **every carveable
subsurface cell** from y=1 to the surface. A mountain stack is roughly
150 cells × 256 columns ≈ 38k calls at 76 ns ≈ 2.9 ms — essentially the
entire measured worst case of `build_column`. Ocean columns skip the carve
and cost 0.24 ms, which is the whole bimodality.

**Fix**: evaluate the two cave bands on a coarse lattice (4×4×4) and
trilinearly interpolate before thresholding — the standard ~64× reduction
used by every voxel engine that does 3D density fields, and the same
structure as the existing 18×18 padded `Fields` grid already used for
column classification (`BENCH.md` fourth pass: 324 evals vs 1280 naive).
Alternatively restrict carving to a depth band below the surface.

Caveat: this changes worldgen output — shared journal `VERSION` bump and
golden-hash re-pin, which per `CLAUDE.md` stales server volumes and client
saves together. Worth batching with any other worldgen retune.

Secondary, same file family:

- **`structures_near` costs 66–71 µs per column job**
  (`decoration.rs:415-452`) and is recomputed 4–9× redundantly across
  neighbouring aprons. `cell_tree`'s pre-gate
  (`decoration.rs:107`, `MAX_TREE_DENSITY_PCT = 55`) admits 55% of cells to
  a full 2 µs `column_shape` even in Sparse (6%) or None biomes; boulder
  and bush candidates each pay their own independent `column_shape` for the
  same cell. Nothing is shared with the 18×18 `Fields` grid the same job
  already computed. Fix: bounded LRU across jobs, and have in-footprint
  candidates read the padded grid instead of re-deriving five fresh
  `fields()` evaluations.
- **`spawn()` (`player.rs:128-186`)** probes `column_shape` per ring
  candidate plus a 24-column `walled` scan plus a full `structures_in` per
  survivor. Ocean-heavy seeds walk many rings — hundreds of ms natively,
  plausibly seconds on wasm, as a one-time startup freeze. Memoize `Fields`
  per column across the walk; presieve rings on the continent field alone.

## 3. Streaming maintenance runs unbudgeted at walking cadence

### 3a. `visibility_scan` is a full-world rescan per chunk crossing

**`pipeline.rs:437-481`, called from `maintain` at `pipeline.rs:407-411`**

Runs whenever the camera crosses a 16-block boundary — every ~16 blocks of
walking — and runs **before** the deadline machinery, so nothing bounds
it. At draw 20 it performs:

- `store.retain` over ~20,000 slots (1257 columns × 16 y),
- 41×41 = 1681 column probes × 16 `entry()` lookups ≈ 27k hash operations,
- a full `mesh_heap` drain-and-rebuild through a fresh `Vec`
  (`pipeline.rs:473-480`).

This is a periodic multi-ms main-thread burst on wasm at exactly walking
cadence, and is the most likely mechanism behind the walking-stutter report
that prompted the urgency-scaled ceiling in `11be34c`. Note the budget
work could not have fixed it — it isn't inside the budget.

**Fix**: incremental rim diff. A single-chunk step changes only the
entering and leaving crescents — O(2r·16) columns instead of O(r²·16).
Drop the mesh-heap re-prioritization entirely: those are priorities, not
correctness, staleness is already handled at pop
(`pipeline.rs:729-759`), and they drift by at most one chunk per crossing.

Related: `gen_heap` is *never* re-prioritized (only `mesh_heap` is), so the
two are already inconsistent — an argument that the rebuild is not
load-bearing.

**Also add eviction hysteresis.** The scan evicts at exactly `draw_dist`
(`pipeline.rs:441`), so a player pacing back and forth across one boundary
line regenerates the same columns forever. Evicting at `draw_dist + 1`
(with the desired set unchanged) is the standard streaming-thrash guard and
costs one ring of memory.

### 3b. Eviction bursts are unbudgeted and quadratic

**`engine.rs:1365-1373`**

A boundary crossing evicts a whole rim arc (600+ chunks at draw 20) in one
frame. Then `pending_uploads.retain(|out| !m.evicted.contains(&out.pos))`
does a **linear scan of the evicted `Vec` per pending element** —
O(pending × evicted) — followed by a `renderer.remove_chunk` per chunk
(two hash removes plus wgpu buffer drops, each a JS-side release on wasm).
None of it counts against `full_deadline`.

**Fix**: make `evicted` an `FxHashSet` for the retain; drain
`remove_chunk` from a pending-evict queue under the shared budget (GPU
buffers merely idle until dropped).

### 3c. The three streaming budgets stack

**`engine.rs:1350-1352`**

```rust
let full_deadline  = t_maintain + budget_ms;
let drain_deadline = t_maintain + budget_ms / 2;
let allowance      = budget_ms;          // inline jobs
```

`maintain` shifts only the *drain* deadline by inline-job time
(`pipeline.rs:419-424`), so worst case is: inline jobs spend up to
`budget` (plus one prediction error), the drain then gets its full
`budget/2` on top, and the upload loop measures against the already-expired
`full_deadline` but still performs `UPLOAD_MIN = 4` uploads regardless.
Total ≈ 1.5 × budget + overshoot + 4 buffer creations — at the 10 ms
ceiling that is 15 ms+ before update and encode, which guarantees the miss
that resets the controller. (Currently masked by §0 pinning the budget at
3 ms; it becomes live the moment §0 is fixed.)

**Fix**: one shared deadline threaded through all three phases, each
consuming from the same remaining allowance.

### 3d. The cost predictor is blind to bimodal gen cost

**`pipeline.rs:176-213`** — worst-of-last-8 per job kind. After eight
consecutive cheap ocean columns the prediction collapses, admitting a
mountain column that costs 10×. Overshoot is bounded to one job, but one
job is a whole column (§1).

**Fix**: blend a lifetime high-water mark
(`max(ring_worst, 0.5 × lifetime_max)`), or predict from the column's own
terrain class — a heightmap probe is cheap and already pure. This is
standard WCET-estimation practice from real-time scheduling: admission
control needs a *pessimistic* bound, not a recent average.

### 3e. Buried chunks re-mesh repeatedly to produce empty meshes

**`pipeline.rs:556-566`** — the comment says it plainly: *"Faces always."*
Face-neighbour re-dirties are unconditional; the `empty_mesh_gen`
optimization only skips *diagonal* bumps.

A fully-buried chunk costs ~97 µs to mesh into nothing (`mesher.rs:509-580`
scans all 4096 cells × 6 directions regardless of output). During fill and
walking, every landed column re-meshes the buried chunks of up to 4
adjacent columns, and each column's buried chunks re-mesh up to ~5× total —
competing with real work inside the inline budget.

**Fix**: store 6 per-face border-occupancy bits per chunk at gen time
("this border plane is fully opaque" / "fully air"). Skip the face
re-dirty when both sides of the shared plane are uniformly opaque (the
buried case) or uniformly air. Only mixed borders can change.

### 3f. `pending_uploads` re-sorted every frame

**`engine.rs:1379-1385`** — `make_contiguous().sort_by_key(...)` whenever
`len > 1`, every frame, even when nothing was appended. During fill or fast
travel this is an O(n log n) shuffle of `MeshOutput` structs (each carrying
several inline `Vec` headers) on frames that are already tight. Sort only
on append or centre change, or keep a `BinaryHeap` keyed at push time.

## 4. Renderer

### 4a. Fresh GPU buffer per chunk per remesh, no pooling

**`renderer.rs:718-770`** — `upload_chunk` calls `remove_chunk` (dropping
the old buffers) then creates brand-new exact-size buffers. An edit remesh
bumps up to 27 neighbours; walking churns the rim continuously. On Chrome
each create/destroy is a GPU-process allocation plus a deferred free, and
wgpu batches destruction into periodic cleanup spikes.

**Fix**: reuse the existing `ChunkGpu` allocation when the new size fits
its capacity (the common case for a same-chunk remesh), and round
allocations to size classes (16 KiB steps) backed by a free-list. The
end-state used by modern engines is a single large vertex arena with
sub-allocation and base-vertex draws, which also unlocks 4c.

### 4b. Upload budget is wall-clock only — and wall-clock lies on web

**`engine.rs:1386-1396`** — the loop stops on `full_deadline` but always
performs `UPLOAD_MIN = 4` uploads regardless of their size, and there is no
byte cap anywhere.

On web this is measuring the wrong thing: `queue.write_buffer` merely
*records* the copy; Chrome's GPU process performs the staging transfer at
`submit`. So `upload_ms` systematically undercounts, and the controller in
§0 would be fed an optimistic signal even after its subtraction bug is
fixed. A frame draining four large meshes pushes a multi-megabyte copy
burst into one submit, invisible to every existing metric.

**Fix**: add a bytes-per-frame ceiling (512 KiB–1 MiB on wasm) checked
alongside the deadline; reduce `UPLOAD_MIN` to "the player's own column"
rather than 4 unconditional uploads.

### 4c. Three full hash-map sweeps of the resident set per frame

- scene pass — `renderer.rs:1031-1047`
- near shadow — `shadow.rs:481-487` (whole map, every frame)
- water list — `water.rs:99-111`

Each is a pointer-chasing `FxHashMap` walk plus a scalar 4-plane frustum
test per chunk, thousands of entries at draw 20, growing as draw-dist².
Worse, hash iteration order is effectively random, so **the scene pass
draws in no spatial order and gets zero early-Z benefit** — pure overdraw
on the immediate-mode GPUs Chrome typically runs on.

**Fix**: maintain a flat `Vec<(ChunkPos, slot, has_water)>` updated on
upload/remove; cull once per frame into a reused scratch buffer shared by
scene and water; sort coarsely front-to-back by column distance when the
camera chunk changes. The plane test is then trivially vectorizable, but
the data structure is the larger win.

### 4d. Per-draw bind-group churn

**`renderer.rs:1037-1043`, `shadow.rs:553-559`, `water.rs:130-137`** — every
chunk draw issues `set_bind_group` (dynamic offset) + `set_vertex_buffer` +
`draw`, i.e. three wasm→JS encoder calls with validation, in every pass —
2–5k triplets per frame on web, and water pays it twice per chunk.

**Fix**: the chunk origin is 16 bytes. Move origins into a storage buffer
indexed by `@builtin(instance_index)` (or an instance-step vertex buffer
with `first_instance` as the slot index) so the loop collapses to
`set_vertex_buffer + draw`. Combined with 4a's arena, adjacent chunks
become contiguous draws.

This is the on-ramp to GPU-driven rendering (Haar & Aaltonen, *GPU-Driven
Rendering Pipelines*, SIGGRAPH 2015): per-chunk metadata in a storage
buffer, frustum culling in a compute shader, `drawIndexedIndirect`.
Core WebGPU lacks multi-draw-indirect today, so the full version is
premature — but the flat array plus instance-indexed origins is the
prerequisite and is worth doing on its own merits.

### 4e. Near shadow cascade re-renders at full cost every frame

**`shadow.rs:450-488`** — no change detection: every frame clears the
2048² near map and redraws every caster in the light frustum. The near VP
is already texel-snapped (`sun.rs:159-187`), which means for a stationary
camera and static scene the map is **bit-identical** frame to frame.

Texel snapping is precisely the precondition for shadow-map caching, which
is standard practice in modern engines for exactly this reason.

**Fix**: skip the near pass (keep the previous map, `LoadOp::Load`) when
`near_vp` is unchanged *and* no chunk upload or removal touched its frustum
since the last render. On web this removes hundreds of draws plus a full
shadow rasterization from the common idle and slow-walk frame.

### 4f. Wide-cascade refresh has no platform knob

**`renderer.rs:185` (`WIDE_SLICE_DRAWS = 1536`), consumed at
`shadow.rs:525`; `engine.rs:178-186` passes the constant on every
platform** — the comment already concedes "no per-platform knob yet".
Every ~3.33 s a refresh frame encodes up to 1536 extra draw triplets and
rasterizes 1536 chunks: a periodic sawtooth measured at 1–2 ms natively and
worse on wasm encode.

**Fix**: expose `wide_slice_draws` per platform (web/mobile 384–512 — the
refresh simply completes over more frames, and the sun moves
imperceptibly), or modulate it by the same headroom signal as the stream
budget once §0 is fixed.

### 4g. Fixed full-screen pass stack runs unconditionally

**`renderer.rs:1068-1089` (depth blit into R32Float), `1123-1126`
(composite blit), `sky.rs:50-53`** — the depth blit exists only for water
shading, SSR, and the highlight decal's manual occlusion, but runs
full-resolution every frame even with no water in view and no highlight
active (caves, deserts, dry menu backdrops). Not a spike, but a constant
bandwidth tax that shrinks the headroom everything else competes for.

**Fix**: skip the depth blit when `water.scratch.is_empty() &&
!highlight.active`.

### 4h. Smaller renderer items

- **`grow_chunk_buf` (`renderer.rs:675-714`)** allocates a new UBO, builds a
  multi-MB zeroed staging `Vec`, does one big `write_buffer` and rebuilds a
  bind group — all in one frame. Rare today thanks to generous initial
  sizing, but a runtime draw-distance increase leans on this path (as its
  own comment notes) and it will land as a single hitch. Grow at ~85%
  occupancy during a frame with headroom, and skip the zero-fill.
- **World-swap burst**: `clear_chunks` (`renderer.rs:784-789`) drops every
  chunk's buffers in one frame, from `reset_world_transients`
  (`engine.rs:868-871`). Semi-acceptable at a menu transition, but on web
  the destroy avalanche bleeds into the first frames of the new world. The
  eviction queue from §3b covers this too.
- **Wide-refresh kickoff** (`shadow.rs:393`) collects all chunk keys into a
  fresh `Vec` every 3.33 s. Retain and reuse it.

**Verified clean** (do not re-flag): one encoder and one `queue.submit` per
frame; `buffer_init` deliberately avoids `create_buffer_init` to dodge the
wasm detached-`ArrayBuffer` trap; GPU timer readback is non-blocking
`map_async` + `PollType::Poll`; no runtime pipeline or shader-module
creation anywhere; the blocking `PollType::Wait` exists only in native
screenshot capture; wide ping-pong bookkeeping is test-pinned and a camera
cut cannot force a full wide re-render.

## 5. Netcode, persistence, audio

### 5a. localStorage save re-serializes the entire world every 5 s

**`apps/web/src/store.rs:107-132`, driven from `maintain` at
`store.rs:312-329`, called per frame from `engine.rs:2565`**

While a local web world is dirty, every 5 seconds the engine walks the
**entire** `EditOverlay` into a fresh `JournalIndex`, serializes it,
base64-encodes it (~1.33× expansion, another `String`), and writes it with
**synchronous** `localStorage.setItem` — inside the frame.

Cost scales with lifetime edit count and grows without bound. A mature save
is a guaranteed recurring hitch on a 5-second cadence, which is the exact
failure mode this audit is hunting, and it lands in `update_ms` with no
finer attribution.

The synchronous-flush constraint (`flush` must run inside
`on_suspend`) applies only to the *flush* path, not the steady-state one.

**Fix**: per-region keys so only dirty regions re-serialize, or an
incremental encode spread across frames into a reused buffer with the
`setItem` last.

### 5b. Net event drain is unbudgeted

**`engine.rs:1104-1297` (`pump_net`) + `net/src/client.rs:184-225`
(`poll`)** — `poll` decodes the entire reliable backlog and every queued
datagram; `pump_net` applies every resulting event, with no deadline.

- **Join sync** is the worst case: the server streams the whole journal as
  `ServerMsg::JournalColumn`, one `NetEvent::Edit` per cell
  (`client.rs:352-363`), and the browser delivers as fast as it reads.
  Thousands of `apply_authoritative` calls, each doing hash lookups plus up
  to 8 `mesh_heap` pushes (`pipeline.rs:871-925`) — a multi-hundred-ms
  hitch at the "Connected" moment, hidden inside `update_ms`.
- **Fluid broadcasts** are the recurring case: a wave arrives as N
  `EditApplied` frames and is applied all at once. Only the *meshing* is
  budgeted; the apply and heap churn are not.

**Fix**: cap events processed per frame, carrying the remainder (they are
per-cell LWW and order-independent; the `deferred_edits` machinery already
exists). Keep `Ack`/`Teleported` exempt — order-sensitive but cheap.

### 5c. Deferred-edit drain is O(total deferred) per landing frame

**`pipeline.rs:981-996`** — `apply_deferred_for` iterates the *entire*
`deferred_edits` map with a linear `landed.contains` per entry, then
collects a fresh `Vec`.

On a heavily-edited server, every edit outside the generated radius defers
and *stays* deferred until the player walks there, so the map holds tens of
thousands of entries for the whole session — and this runs on every frame
that lands a column during streaming.

**Fix**: key by column — `FxHashMap<(cx, cz), Vec<((x,y,z), u16)>>` — making
the drain O(landed).

### 5d. Shape-variant bake spikes on first use

**`shapes.rs:407-481`** — three independent `LazyLock`s. Measured natively:
first `shape_of` on a door **720 µs** (bakes all 16 variants — each
rasterizes 1728 cells plus 6×12 layer scans plus greedy merge); stairs
207 µs. Expect ~1.5–3 ms on wasm. Triggered mid-gameplay the first time a
door or stair appears in a mesh input, a `neighbor_cover` test, a physics
probe, or the alchemy screen. Warm calls are 2.5 ns.

**Fix**: force-init all three during engine startup — three `shape_of`
calls. This is the same discipline as PSO precompilation: no first-use
surprises after the loading screen.

### 5e. Ambient audio evaluates the full terrain noise stack per frame

**`engine.rs:802-815`** — `step_audio` calls
`worldgen::terrain_height(cam.x, cam.z, seed)` every frame whenever the
camera isn't submerged, running the whole continentalness/roughness/ridge/
detail field evaluation to answer "am I 6 blocks under grade?". It is a
pure function of the column, and the camera changes column a few times per
second at most. Cache by `(bx, bz)`, or fold into the existing 10 Hz
water-audio cadence.

### 5f. WebTransport JS-allocation churn

- **Receive** (`net-web/src/transport.rs:126-157`): each read allocates two
  **new interned JS strings** for the `Reflect::get` keys `"done"` and
  `"value"`, registers per-promise closures, and copies to a fresh `Vec` —
  roughly 50–150 JS allocations/second feeding Chrome's GC, and GC pauses
  land as frame spikes. Cache the key `JsValue`s in a `thread_local`; copy
  datagrams into a reused wasm buffer (bounded by `MAX_DATAGRAM = 1150`).
- **Send** (`transport.rs:253-258`): a new `Uint8Array` plus two promises
  per send, ~30/s steady. The shared `noop_catch` closure is already
  correct. There is no backpressure signal at all (`transport.rs:267-271`) —
  consider polling `writer.desiredSize` and coalescing datagram sends when
  negative.

### 5g. Steady allocation churn (low severity, batch opportunistically)

- `engine.rs:2476-2485` — two `Vec` allocations **every connected frame**
  purely to `!=` against `last_layout_sent`. `to_wire()` returns arrays;
  compare slices and allocate only when sending.
- `engine.rs:2737-2740` — a `String` clone per nearby remote player per
  frame for name tags, plus a `format!` fallback.
- `net/src/client.rs:499,557` + `codec.rs:32,45` + `predict.rs:111-131` —
  three allocations per outbound message across the layers; a persistent
  scratch buffer with `postcard::to_slice` removes them.
- `codec.rs:76-91` — a `Vec` allocation and copy per reliable frame body;
  a borrowed `next_frame() -> Option<&[u8]>` would decode in place.
- `overlay.rs:357-365` — six `String` clones per frame for the debug panel
  (debug-only).
- `mesher.rs:569-574` — `tint` hashed per *face* rather than per cell (up to
  6× redundant); the shaped path already hoists it.

### 5h. Ocean-surface mesh jobs pay a quadratic splash scan

**`mesher/water.rs:169-186`** — every open-surface, non-falling water cell
scans a 5×5 neighbourhood × 2 layers with `is_impact` (2 reads each) —
50–100 padded reads *per water cell*, so a 256-cell ocean surface chunk
pays 13–25k reads, comparable to the entire solid pass. Almost all chunks
contain zero falling water. Add a cheap "any falling cell present" flag
accumulated during `MeshInput::build` and skip all ring scans when absent.

**Verified clean**: remote-player interpolation (`remote.rs:80-126` — reused
scratch, no sorting); pending-edit bookkeeping; menu services (self-
throttled at 150 ms, nothing polled per frame); fluid tick bounded by
`FLOW_BUDGET = 128` with identical client/server caps; rewind/replay
bounded at `BUF_CAP = 128` ticks (24 µs measured with a trivial world
closure, ~1 ms wasm worst case through real hash probes — real but rare,
only on hard divergence); `reconcile_layout` at O(36×10); raycast DDA and
`overlaps_solid` probe bounded cell counts with closed-form tests.

## 6. You cannot see web spikes today

Two instrumentation gaps make everything above harder than it needs to be,
and fixing them should probably come *first*.

- **`DIG_SPIKELOG` cannot be enabled on web** (`metrics.rs:281` uses
  `std::env::var_os`, which always returns `None` on wasm). The per-spike
  phase-attribution tool is unavailable on the one platform where
  main-thread spikes matter most. Make it an `EngineConfig`/`Metrics` flag
  the web launcher sets from `?spikelog` — the engine is otherwise env-free
  by convention, so this env read in core is the outlier anyway.
- **An unattributed phase gap.** Between `pump_pipeline` (end of
  `upload_ms`) and `t_encode` sits untimed work: remote sampling and
  instance build, name-tag projection, menu `view()` while open, the full UI
  `DrawList`/`HitMap` rebuild on inventory and alchemy screens, and
  `frame_uniforms` (which includes a matrix inverse). Likewise `pump_net`,
  fluid stepping and the store `maintain` (§5a) all collapse into
  `update_ms`. A spike in any of these shows up in `total_ms` and in *no*
  phase. Add `prep_ms`/`ui_ms`, and split `update_ms` into sim/net/persist
  at least in the spike log.
- **`pending_uploads.len()` is not in `PipelineGauges`**
  (`engine.rs:1405-1410`) — the engine-side upload backlog is invisible;
  only the channel-side `saturated` flag is reported.
- **`HITCH_MS = 33.4`** (`metrics.rs:11`) means a 60 Hz single dropped frame
  (16.7 → 33.3) never counts as a hitch at runtime. The bench summary
  already reports `over-8.3` / `over-12.5`; the runtime overlay and CSV
  should carry the same, fed from the measured display rate.

Beyond those: there is no `just bench` equivalent for web. Everything in
`BENCH.md` is native. A scripted web flight (the `auto_join` path already
walks the real menu) that dumps per-frame timings to the console for
offline percentile analysis would turn this whole document from
static analysis into measurement.

---

## Recommended order

Grouped so that each stage is independently shippable and measurable.

**Stage 0 — see the problem (do first).**
Web spike log flag, `prep_ms`/`ui_ms` split, upload-backlog gauge, a
sub-vsync hitch counter, and a scripted web bench flight. Everything
downstream is guesswork without it.

**Stage 1 — the two structural web spikes.**
1. Fix the busy-time signal (§0) — small diff, unblocks a shipped feature.
2. Unify the three streaming deadlines (§3c) — land with 1, since 1 makes
   the stacking live.
3. Slice the column-gen job into continuations (§1) — the largest single
   improvement to worst-frame on wasm.
4. Prewarm the shape bakes (§5d) — three lines.

**Stage 2 — the periodic hitches.**
5. Incremental rim diff in `visibility_scan` + eviction hysteresis (§3a).
6. Spread eviction, hash-set the retain (§3b).
7. Incremental/regional localStorage save (§5a).
8. Budget the net-event drain (§5b) and index deferred edits by column
   (§5c).

**Stage 3 — steady-cost reductions that widen the headroom.**
9. Coarse-lattice cave noise (§2) — biggest raw win, but batch it with any
   other worldgen retune since it forces a `VERSION` bump.
10. Border-occupancy masks to kill buried remeshes (§3e).
11. Upload byte budget (§4b), chunk buffer pooling (§4a).
12. Near-shadow caching (§4e), per-platform wide slice (§4f), conditional
    depth blit (§4g).
13. Flat culled chunk list, front-to-back order, instance-indexed origins
    (§4c, §4d).

**Stage 4 — opportunistic.**
Allocation churn (§5g), splash-scan flag (§5h), structure apron memoization
(§2), ambient-audio column cache (§5e), WebTransport key caching (§5f).

---

## Techniques worth borrowing

Cross-cutting principles behind the specific fixes above.

**Measure occupancy, not wall time.** §0 is an instance of a general trap:
a derived metric that is correct on one platform silently inverts on
another. On web the authoritative main-thread cost is rAF-entry to
post-submit; anything involving the frame delta includes the browser's
idle. Prefer directly measured intervals over arithmetic on two other
intervals.

**Nothing unbounded per frame, ever.** The recurring shape of this audit is
work that is *usually* small: the visibility scan, the eviction burst, the
net drain, the deferred-edit sweep, the localStorage snapshot. Each is
fine in the common case and terrible in the tail, and the tail is what
frame consistency means. Every one of them wants either an incremental
formulation or a budget with carry-over.

**Long jobs must be able to yield.** The column-gen job is the classic
long pole. Naughty Dog's fiber system (Gyrling, GDC 2015) exists to make
every job preemptible at a known point; without threads the wasm-side
equivalent is explicit continuations. A scheduler can only ever be as good
as its most indivisible unit of work.

**Admission control needs pessimistic bounds.** Worst-of-8 is an average
dressed as a bound (§3d). Real-time scheduling uses WCET precisely because
a predictor that tracks recent history admits the outlier it should have
excluded.

**Hysteresis on every boundary.** Evict at `draw_dist + 1` while desiring
`draw_dist` (§3a); the controller in §0 needs the same treatment on the
120 Hz sawtooth. Any system that changes state exactly at a threshold will
oscillate when the input sits near it.

**Coarse lattice + interpolation for 3D density fields** (§2) — the
standard ~64× reduction, and the same trick this codebase already applies
successfully to column classification on the 18×18 grid.

**Texel-snapped cascades exist to be cached** (§4e). The snapping is
already done; the caching is the payoff. Static-scene frames should not
re-rasterize an identical shadow map.

**Flat arrays beat hash maps in per-frame iteration** (§4c) — not merely
for the constant factor, but because iteration order becomes controllable,
which is what buys front-to-back early-Z.

**Move per-draw constants to indexed storage** (§4d) — the well-trodden
path toward GPU-driven rendering (Haar & Aaltonen, SIGGRAPH 2015). Worth
doing for the encoder-cost reduction alone even though core WebGPU cannot
yet do multi-draw-indirect.

**No first-use surprises after the loading screen** (§5d) — lazy
initialization is a correctness convenience and a latency hazard; warm
every cache during load. The industry's expensive lesson here is shader/PSO
compilation stutter.

**Deferred work has deferred cost** (§4b) — `write_buffer` returning
quickly does not mean the copy was cheap. When the real cost lands in
another process at submit time, budget the *bytes* you hand over, not the
time it took to hand them over.

---

## Implementation notes (2026-08-06)

### The one deferred item: column-gen slicing (§1)

The audit reasoned about this from the `CostRing::new(12.0)` seed and a
2.9 ms native worst case, concluding a column-gen job could exceed a whole
frame on wasm and that no budget could help.

Measured after the fact, in Chrome on Apple Silicon at draw 20: **gen jobs
run 1.17 ms average, 1.70 ms maximum.** The bit-exact cave-noise work
(lattice-corner reuse across a column's vertical walk) moved the number by
roughly 3–4×, and the 12 ms seed turns out to have been a conservative
placeholder rather than a measurement. A 1.7 ms indivisible unit fits
inside a frame with room to spare, so slicing `Job::Generate` into
resumable continuations would now be a substantial refactor of worldgen's
column path for a small and possibly unmeasurable win.

It is worth revisiting only if gen cost shows up as the tall pole on
weaker hardware — the new `?spikelog` output attributes it directly, and
`maintain_ms` in the CSV bounds it. Do not do this refactor without that
evidence first.

### Things measurement contradicted

Worth recording, because two of this document's confident claims were
wrong in ways only running the code revealed:

- **Per-cell tint hoisting (§5g)** was predicted to save up to 6× redundant
  hashing. Measured on a worst-case chunk: 386 µs before, 388 µs after —
  LLVM was already hoisting it out of the direction loop. The change was
  kept for explicitness, not for speed.
- **Near-cascade caching (§4e)** assumed the texel snap made the near VP
  stable for a still camera. The snap covers translation only; the
  rotation follows `tod`, which advances every frame, so the cache would
  never have fired in play. Fixed by quantizing the caster angle fed to
  the near cascade (`NEAR_TOD_STEP_DEG = 0.25°`, ≤ ~1 texel of edge step,
  ~1 re-rasterization per second instead of ~120). Note it still only
  pays off while the player is stationary — the texel-snapped translation
  changes every frame at walking speed.

### Design choices that diverged from the recommendation

- **§4c front-to-back ordering** was implemented as an O(n) counting sort
  into distance buckets every frame, not a comparison sort on camera-chunk
  change. A sort triggered by chunk crossings would put a ~0.3 ms spike at
  exactly the boundary-crossing moments this whole pass exists to smooth.
- **§4d indexed origins** used an instance-step vertex buffer rather than
  a storage buffer indexed by `instance_index`. Both need non-zero
  `first_instance` (fine for direct draws in core WebGPU), but the storage
  form additionally needs storage buffers in the vertex stage, and this
  codebase has none anywhere — untested ground on Chrome for no benefit.
  Bind group 3 is gone from all three pipeline layouts either way, and the
  data layout stays a flat slot-indexed array, so the storage-buffer
  version remains a one-line change if GPU-driven culling ever wants it.
- **Journal sharding (§5a)** kept `VERSION` at 3. The record format did not
  change — only the container — so existing worlds MIGRATE rather than
  stale. That was worth the extra migration code: bumping the version
  would have destroyed every existing server world and local save for a
  pure performance change.
