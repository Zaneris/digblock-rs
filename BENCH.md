# Bench baseline & hotspot analysis

## 2026-08-09 — the server gets a jitter buffer (`INPUT_HOLD_MAX`)

Acting on the measurement pass below. The drain now refuses to step past a
**hole** in the input tick sequence for up to `INPUT_HOLD_MAX` ticks, so an
input reordered by jitter is still admitted when it lands instead of arriving
under the high-water mark and dying there. `EDIT_POS_TOLERANCE` and
`dig_grace` are deliberately **not** made depth-aware — see the rule on
`INPUT_HOLD_MAX` in `protocol.rs`.

The bench grew a second link table for this, because the constants a hold
spends are edit-path constants and the sweep only ever walked.

### Before (`INPUT_HOLD_MAX = 0`) → after (`= 4`)

| link | in-deliv | late/s | rtt mean | repl/s | corr mean | corr max | depth | orig max |
|---|---|---|---|---|---|---|---|---|
| lan | 100.0 → **100.0** | 0.0 → 0.0 | 26.0 → 26.0 | 0.00 → 0.00 | 0.000 → 0.000 | 0.00 → 0.00 | 1.5 → 1.5 | 0.000 → 0.000 |
| wifi | 99.9 → **99.9** | 0.0 → 0.0 | 48.1 → 48.1 | 2.70 → 2.70 | 0.069 → 0.069 | 0.13 → 0.13 | 1.5 → 1.5 | 0.067 → 0.067 |
| cellular | 98.9 → **99.8** | 0.6 → 0.0 | 142.1 → 142.8 | 3.85 → 3.85 | 0.078 → 0.078 | 0.20 → 0.20 | 1.7 → 1.8 | 0.266 → 0.267 |
| bad-4g | 91.8 → **98.0** | 8.2 → 1.8 | 299.5 → 310.4 | 3.85 → 3.65 | 0.093 → 0.090 | 0.33 → 0.33 | 1.9 → 2.5 | 0.599 → **0.399** |
| awful | 73.0 → **89.5** | 26.3 → 8.9 | 430.2 → 474.3 | 5.40 → 4.65 | 0.273 → 0.220 | 0.73 → **1.07** | 1.7 → 4.6 | 1.021 → **0.867** |

**The lan and wifi rows are unchanged to the digit.** Not approximately —
identically, every column. A link with no reordering has no holes, so the
branch never fires. That is the whole cost argument: almost every player pays
exactly nothing for this.

**A hold does not spend `EDIT_POS_TOLERANCE` — it saves it.** This was the
prediction the sweep refuted, and it is the most useful thing it found. The
arithmetic said a deeper buffer must push the claimed edit origin further
from the server's simulated position, at 0.067 blocks per tick of walk. The
measurement says `orig max` *falls*: 0.60 → 0.40 at 300 ms, 1.02 → 0.87 at
500 ms. The reason is that the alternative to holding is coasting on stale
intent, and a coast diverges faster than a delay does. The tolerance headroom
was never the binding constraint at this cap.

**What it costs is latency, and only on links that already have some**:
+0.7 ms at 120 ms, +11 ms at 300 ms, +44 ms at 500 ms. Client prediction
hides all of it for movement; it is visible only where the server's current
position is compared against the client's, which is the `orig` column above —
and that got better, not worse.

### Honest debits

`corr max` at 500 ms goes the wrong way: 0.73 → 1.07 blocks. It is one
deterministic event (identical at caps 2, 4, 6 and 8), and it is the tail of
the mechanism — when a hole never fills, the server coasts the full cap and
then skips, which is a single larger divergence than skipping immediately.
The body of the distribution improves: `corr mean` −19% and `repl/s` −14%, so
total corrective motion falls from 1.47 to 1.02 blocks/s. Fewer, smaller
corrections with one rarer, bigger one — worth it, but not the unambiguous
win the acceptance criterion assumed.

`dig ok/att` at 500 ms reads 7/8 → 6/8. **This column cannot rank the cap**:
stone is 90 ticks, so a 20 s row gets 8 attempts and the sweep bounces around
4–8 with no trend. `dig short` is zero on every row at every cap, so
`dig_grace` is not what is failing — the abandonments are
`DIG_OFF_TARGET_RESET`, the server's `look` lagging far enough at 500 ms that
a hold exceeds 30 consecutive off-target ticks. That is the known
aim-has-no-adoption-envelope problem, not a regression from this change, but
a longer-window dig measurement would be needed to say whether a hold makes
it meaningfully worse.

### The sweep the cap was chosen from

`in-deliv` by cap:

| cap | 0 | 2 | 4 | 6 | 8 | 12 |
|---|---|---|---|---|---|---|
| cellular | 98.9 | 99.8 | 99.8 | 99.8 | 99.8 | 99.8 |
| bad-4g | 91.8 | 96.5 | **98.0** | 98.2 | 98.2 | 98.2 |
| awful | 73.0 | 82.5 | **89.5** | 93.0 | 94.3 | 93.7 |

and what the higher caps cost, on the `awful` row:

| cap | 0 | 2 | 4 | 6 | 8 | 12 |
|---|---|---|---|---|---|---|
| rtt mean | 430 | 455 | 474 | 484 | 487 | 488 |
| repl/s | 5.40 | 4.45 | **4.65** | 5.10 | 5.95 | 6.25 |
| orig max | 1.021 | 0.799 | **0.867** | 1.132 | 1.328 | 1.787 |
| tol-rej/s | 0 | 0 | 0 | 0 | 0 | **0.05** |

4 is the knee. bad-4g is done there — 6 and 8 add two tenths of a point
between them. awful keeps improving to 8, but buys it with a replay rate that
climbs back *above* the no-hold baseline, which is the symptom the change
exists to remove. And 12 is where the arithmetic finally bites: `orig max`
1.79 exceeds the 1.5 tolerance and honest edits start bouncing — the only
non-zero `tol-rej/s` anywhere in the sweep. The bench catches the failure mode
before a player would.

Both halves stay deterministic: the link and edit tables are bit-identical
run to run. The capacity table is wall-clock and is not — `tick us` swings
~30% with machine load — but a held tick does strictly less work than a
consumed one, so the capacity conclusion below is untouched.

## 2026-08-09 — netcode measurement pass (`just bench-net`)

**This supersedes the baseline below it**, which was partly measuring the
benchmark. Nothing in the engine changed: the harness was fixed, the server
input path was instrumented, and the numbers were re-taken. Recorded as a
second entry rather than an edit because the corrections are the finding.

### What was wrong with the first baseline

| column | defect | effect |
|---|---|---|
| `seen rtt` | the wire delivered once per tick, checked against the same `now` the send used, so `at = now + one_way` was never `<= now` | every leg cost a whole 16.67 ms tick regardless of the configured link |
| `snap/s`, `replay/s` | read from `StatsAcc`, which publishes a closed 1-second bucket | a phase of the run, not the run — wifi printed 20.7/s against a 20.0 nominal |
| `seen rtt`, `+-jit` | EWMA seeded during warmup and never reset | the 4 s startup transient rode into the reported value |
| `alloc/t` | counters bracketed the whole loop body while the timer bracketed only `tick_once` | the harness's own `Vec<SimEvent>` and per-client `vec![]` were being reported as the server's — ~74% of the number at n=64 |
| `tick us` | claims were `[0, -500, 0]`, so the `sane` chain short-circuited on `POS_TOLERANCE` | `probe_solid` → `overlaps_solid`, the most expensive call in the input path, was never run |

The link half now sub-steps the wire and the client 8× per server tick
(2.08 ms). The server still ticks at 60 Hz — a packet arriving mid-tick
genuinely waits for the next boundary, and that quantization is real. Every
link column is measured over the window rather than sampled from a rate
bucket, and clients claim a position predicted with the server's own physics
and re-seeded from server state each tick, so adoption really happens (the
harness asserts it rather than assuming it).

### Server capacity — real worldgen, everyone walking, 600 ticks/row

```
  n |    tick us: p50    p95    p99    max | snap B | kB/s/c | alloc/t   alloc | snap
  1 |    0.8    1.2    1.8   25.1 |   31.0 |    0.6 |     0.6      88K |  100%
  8 |    4.7    7.9   20.2  132.8 |  194.1 |    3.8 |     3.3    1796K |  100%
 24 |   12.9   44.5   65.9   84.5 |  567.0 |   11.1 |     9.3    7402K |  100%
 40 |   18.2   72.2  114.9  120.6 |  939.0 |   18.4 |    14.9   13969K |  100%
 64 |   29.1  138.0  199.8  208.5 |  937.3 |   18.4 |    23.1   19118K |  100%
```

p99 at 64 players is 200 us against a 16 667 us budget — 1.2%. The
conclusion from the first baseline survives: interest management, not tick
cost, is what bounds player count.

`alloc/t` at 64 players is **23.1, not the 88.6 first reported** — three
quarters of that figure was the harness allocating its own input events.
What remains is ~0.36 allocations per client per tick.

Adding the adoption path did **not** move `tick us` measurably. That is
itself a result: `overlaps_solid` is cheap next to residency and snapshot
assembly, so the per-client cost is dominated by interest management rather
than by validating movement.

### Link quality — real NetClient vs real Sim over a seeded impairing wire

Flat world on purpose: this half measures the protocol, and real worldgen
would put worker-thread completion times into the loop. Rows are
bit-identical run to run, so a difference is a difference in the netcode.

```
link     | cfg rtt  loss | drop | rtt mean  p95 | snap/s | repl/s | corr mean  max | max div | in-deliv | late/s want | depth
lan      |     4ms    0% |   0% |    26.0   35.4 |   20.0 |   0.00 |     0.000 0.00 |    0.07 |   100.0% |   0.0    0 |  1.5
wifi     |    30ms    1% |   1% |    48.1   70.8 |   19.8 |   2.70 |     0.069 0.13 |    0.33 |    99.9% |   0.0    0 |  1.5
cellular |   120ms    3% |   4% |   142.1  193.7 |   19.4 |   3.85 |     0.078 0.20 |    0.67 |    98.9% |   0.6    5 |  1.7
bad-4g   |   300ms    5% |   6% |   299.5  385.5 |   18.9 |   3.85 |     0.093 0.33 |    1.06 |    91.8% |   8.2   10 |  1.9
awful    |   500ms   10% |  12% |   430.2  554.2 |   17.8 |   5.40 |     0.273 0.73 |    1.81 |    73.0% |  26.3   18 |  1.7
```

**The `lan` row's RTT is 26.0 ms, not 41.9.** The old figure was ~16 ms of
harness quantization. 26.0 lands next to the 24.8 ms measured live against a
real QUIC server, which is the independent check that the model is now
honest. `seen rtt` still exceeds the wire on purpose — it is the input→ack
loop, so it carries the server's jitter buffer, the 60 Hz tick boundary and
the snapshot cadence, which is the latency a player feels. It does **not**
carry the send stride: `sent_at` is stamped at transmission
(`predict.rs:154`), so a tick waiting its turn on the wire is felt but never
counted. The first baseline claimed otherwise.

**`depth` is the finding.** The server's jitter buffer holds ~1.5 ticks on a
clean LAN and never reaches 2 on any row — including the calm ones. The
drain in `step_physics` takes at least one input whenever the buffer is
non-empty and has no way to take zero, so depth is a one-way ratchet: an
underrun destroys a tick of it permanently and nothing puts it back. The
first baseline said `JITTER_BUFFER_TICKS = 2` was "too small to absorb
±150 ms". That was the wrong diagnosis — the constant is not the mechanism,
and raising it would change nothing. Being able to *hold* would.

**`late/s` and `want` corroborate `in-deliv` from the server's own side.**
Late-lost inputs — arrived below the high-water mark having never been held,
so lost for good rather than being ordinary redundancy copies — are zero on
lan and wifi and climb to 26/s at 500 ms/10%. `want` says how deep the buffer
would have had to be to still be holding the worst of them: **10 ticks at
bad-4g and 18 at awful**, i.e. 167–300 ms. Any fix aimed at those rows has to
be willing to buy that much depth, and every tick of it is latency paid by
that player on every input — plus grace handed to anyone who fakes jitter,
since `dig_grace` and `EDIT_POS_TOLERANCE` are both sized against this lag.

Corrections stay small throughout (max 0.73 blocks at 500 ms/10%) and are
smoothed into the camera over ~200 ms, so the adoption envelope is holding
well past any link this game will realistically see. That is the reason the
delivery shortfall is a polish item rather than an emergency: a dropped input
is mostly overwritten by the next adopted claim. What the envelope does *not*
repair is `client.look`, which is written only from a consumed input and is
what `verify_digs` re-raycasts from every tick — so lost inputs degrade the
server's aim, not just its position.

## 2026-08-09 — netcode baseline (SUPERSEDED, see above)

First multiplayer numbers in this file; the netcode had none. Apple
Silicon (8P+4E), release build. Two halves, because they answer different
questions: server COST, and what a session FEELS like.

### Server capacity — real worldgen, everyone walking, 600 ticks/row

```
  n |    tick us: p50    p95    p99    max | snap B | kB/s/c | alloc/t   alloc | snap
  1 |    0.8    1.3    1.8   16.6 |   31.0 |    0.6 |     2.8     111K |  100%
  8 |    5.6    9.9   23.8   53.3 |  192.5 |    3.8 |    12.5    2444K |  100%
 24 |   11.0   40.2   57.8   70.3 |  561.8 |   11.0 |    34.6   10469K |  100%
 40 |   19.9   92.4  149.6  192.8 |  930.2 |   18.2 |    56.4   19385K |  100%
 64 |   31.5  199.1  271.6  342.0 |  930.3 |   18.2 |    88.6   26669K |  100%
```

p99 at 64 players is 272 us against a 16 667 us tick budget — 1.6%. The
sim thread is nowhere near its limit; interest management, not tick cost,
is what bounds player count. `snap B` flattening at 40 IS
`MAX_SNAPSHOT_PLAYERS` working. Per-client downstream tracks the v14
measurements (24 players: 562 B/snapshot and 11.0 kB/s here against 539 B
and 10.8 kB/s measured standalone) — an independent confirmation of that
change rather than a restatement of it.

### Link quality — real NetClient vs real Sim over a seeded impairing wire

Flat world on purpose: this half measures the protocol, and real
worldgen would put worker-thread completion times into the loop. Rows are
bit-identical run to run, so a difference is a difference in the netcode.

```
link     | cfg rtt  loss | seen rtt  +-jit | snap/s | replay/s | corr mean  max | max div | in-deliv
lan      |     4ms    0% |    41.9     8.5 |   19.7 |     0.00 |     0.000 0.00 |    0.13 |   100.0%
wifi     |    30ms    1% |    56.7    11.1 |   20.7 |     3.93 |     0.067 0.07 |    0.33 |   100.0%
cellular |   120ms    3% |   154.6    20.7 |   18.7 |     5.90 |     0.075 0.20 |    0.67 |    99.4%
bad-4g   |   300ms    5% |   306.7    38.4 |   19.7 |     5.90 |     0.132 0.79 |    1.51 |    91.7%
awful    |   500ms   10% |   415.0    65.0 |   17.7 |     4.92 |     0.243 0.80 |    1.78 |    74.4%
```

Two things worth reading carefully.

**`seen rtt` exceeds the wire RTT by ~38 ms, and should.** It is the
input→ack loop: send stride (0-33 ms) + the server's jitter buffer
(33 ms) + snapshot cadence (0-50 ms). That is the latency a player feels,
which is the number worth showing; a ping would report the 4 ms and
explain nothing. Confirmed live against a real QUIC server on loopback at
24.8 +- 8.5 ms.

**`in-deliv` falls off on the last two rows, and JITTER causes it, not
loss.** Heavy jitter reorders datagrams; `handle_inputs` skips anything at
or below `last_input_tick`, a high-water mark, so a tick arriving after a
newer one was consumed is discarded rather than applied out of order. At
`JITTER_BUFFER_TICKS` = 2 (33 ms) the buffer cannot absorb +-150 ms of
reordering. Deepening it would fix these rows and cost every other player
latency — a tradeoff to make deliberately and adaptively, on measured
jitter, not by raising a constant. Left as a finding.

Corrections stay small throughout (max 0.80 blocks at 500 ms/10%), and
they are smoothed into the camera over ~200 ms, so the adoption envelope
is holding up well past any link this game will realistically see.


All numbers: Apple Silicon (12-core: 8P+4E), release build, `just bench`
(seed 42, tod 45, 1800 frames, scripted flight, inputs off), ×3 runs.

## 2026-08-06 — frame-consistency pass (audit in `PERF-AUDIT.md`)

A full hot-path audit aimed at spike elimination, weighted to the web
build. Findings and the reasoning behind each fix live in
`PERF-AUDIT.md`; this entry records what changed and what was measured.

### Desktop A/B (vsync, seed 42, ×3 each, same machine and session)

Baseline is a `git worktree` at the pre-pass commit, benched interleaved
with the new build so machine state is shared.

| metric | before (×3) | after (×3) | |
|---|---|---|---|
| fill | 4.77 / 4.56 / 4.73 s | 4.02 / 4.20 / 4.23 s | **−11%** |
| avg | 8.34 / 8.35 / 8.35 | 8.36 / 8.36 / 8.36 | flat (vsync-bound) |
| p95 | 9.22 / 9.25 / 9.15 | 8.79 / 8.80 / 8.79 | **−4.7%** |
| p99 | 9.89 / 9.97 / 9.96 | 9.33 / 9.17 / 9.35 | **−6.3%** |
| worst | 17.0 / 17.0 / 16.7 | 24.7 / 17.2 / 17.3 | comparable |
| over-12.5ms | 2 / 3 / 3 | 4 / 5 / 5 | see below |
| gen job avg | 1.37 ms | 1.04 ms | **−24%** |
| mesh jobs | 81992 | 75676 | **−7.7%** |
| resident | 19920 | 20704 | +3.9% (hysteresis band) |

**p95 and p99 are the real signal**: they moved by more than their
run-to-run spread (p95 varies by 0.01 ms across the three new runs versus
a 0.43 ms gap to baseline), and fill improved consistently.

**`over-12.5ms` went slightly UP (2–3 → 4–5) and is reported as-is.** Two
extra frames out of 1799 is not a defensible regression signal at this
sample size, and the third-pass work above already established that these
counts match a static-scene control — they are compositor noise on this
machine, not app-caused. `over-8.3ms` (568 → 648) is likewise not
meaningful: avg sits at 8.36 against an 8.33 threshold, so a 0.02 ms shift
in the mean moves hundreds of frames across the line. Neither number
should be read as a tail regression, and neither should be read as a win.
If the tail matters, judge it on p99.

The web build is where this pass was aimed, and it has no scripted bench;
overlay readings are below.

### The headline: the adaptive stream budget was inert on web

`StreamBudget` derived busy time as `total_ms − acquire_ms`. That is
correct only where vsync blocks inside the swapchain acquire. On the web
`GPUCanvasContext.getCurrentTexture()` cannot block (it would deadlock the
JS main thread) and rAF paces the loop BETWEEN frames, so acquire read ~0
while `total_ms` was always a full display interval — headroom was
permanently negative and the budget pinned to its 3 ms floor forever, on
the one platform the feature exists for. At 120 Hz it was bistable
instead: full ceiling while work fit one 8.3 ms slot, hard collapse the
moment it spilled into two, i.e. a ~0.5–1 s sawtooth.

Verified by running the shipped controller directly (floor 3, ceiling 10,
600 frames, urgency 1.0):

| signal shape | frame ms | busy/acq | budget reached |
|---|---|---|---|
| native FIFO, 5 ms work | 16.7 | acq 11.7 | 10.00 (ceiling) |
| web rAF, 2 ms work | 16.7 | acq 0 | **3.00 (floor)** |
| web rAF, 8 ms work | 16.7 | acq 0 | **3.00 (floor)** |
| web rAF, true busy fed | 16.7 | busy 8 | 10.00 (ceiling) |

The existing test `near_miss_collapses_to_floor` had been asserting
exactly this shape (`on_frame(16.4, 0.4, …)` → floor) without anyone
noticing it *was* the web steady state.

Fix: occupancy is now MEASURED — `Game::frame_started` opens a span at
redraw-handler entry that `render` closes, minus the acquire wait, which
is right on both platforms. `web_signal_shape_still_grows` and
`high_refresh_web_tracks_work_not_quantized_delta` pin it.

### First web measurements (Chrome/WebGPU, Apple Silicon, 120Hz)

`?overlay&spikelog&seed=42`, backdrop world at the menu, draw 20. There is
still no scripted web bench — these are read off the runtime overlay — but
they are the first web frame numbers recorded here at all.

| state | fps | avg | p95 | p99 | worst | hitch | budget |
|---|---|---|---|---|---|---|---|
| streaming in | 119.9 | 8.34 | 12.30 | 12.80 | 13.30 | 0 | **5.6** |
| quiesced | 120.0 | 8.33 | 9.40 | 9.60 | 9.70 | 0 | 3.0 |

Two things to read off that table:

- **`acq 0.02 ms`** — the acquire wait really is ~0 on the web, which is
  the whole premise of the bug above. The old `total − acquire` derivation
  therefore reported ~16.7 ms of "busy" on a frame doing 3 ms of work.
- **`budget 5.6`** while streaming — the controller now grows past its
  3.0 floor, which it was structurally incapable of doing before. It
  returns to 3.0 once the world quiesces because urgency drops to zero
  with nothing pending; that part is by design (rim-only churn streams at
  the floor).

Per-job costs on this machine under wasm: gen 1.17 ms avg / 1.70 ms max,
mesh 0.11 ms avg / 0.40 ms max. The gen figure is far below the 12 ms
`CostRing` seed that the audit's worst-case reasoning was built on — the
bit-exact cave-noise work moved it — so job slicing (the one deferred
milestone) is now a smaller win than the audit projected.

### Measured wins

**Face re-dirty skips on the arrival path** (`pipeline.rs`). The
empty-mesh and queued-remesh skips applied only to diagonal neighbours;
they are equally sound for faces *on arrival*, because a chunk landing
turns all-air shell into real cells, so coverage only increases and a
neighbour that already meshed to nothing stays nothing. (Not true for
edits — digging removes coverage — so `set_cell` keeps its unconditional
bump.) Fill census, workers 4:

| seed / draw | mesh jobs before | after | mesh ms before | after |
|---|---|---|---|---|
| 42 / 8 | 3567 | 3328 (−6.7%) | 486 | 369 (−24%) |
| 42 / 12 | 10212 | 9181 (−10.1%) | 997 | 915 (−8.2%) |
| 1234.5678 / 12 | 7493 | 6456 (−13.8%) | 646 | 576 (−11%) |

**Mesher splash-ring skip**: full ocean-surface chunk 79.7 → 65.6 µs
(−18%), waterfall chunks unchanged, pinned byte-identical by
`water_heavy_chunk_mesh_is_pinned`. The per-cell tint hoist in the same
pass measured as **no change** (LLVM was already hoisting it) — kept for
explicitness, not for speed.

**Reliable frame encoding** 110 → 27 ns (4×) via in-place `postcard::to_io`
instead of an intermediate Vec; benefits the server equally.

### Structural changes without a single-number result

- `visibility_scan` was a full-world rescan on every chunk crossing —
  ~20k retain slots + ~27k hash probes + a full mesh-heap rebuild at draw
  20, running OUTSIDE the streaming deadline, i.e. an unbudgeted burst at
  walking cadence. Now a scanline interval diff of the entering/leaving
  crescents: O(r) instead of O(r²). Eviction gained one chunk of
  hysteresis (`KEEP_MARGIN`) so pacing across a single boundary stops
  regenerating the same crescent — costs ~10% more resident columns at
  draw 20. Pinned by `incremental_scan_holds_the_residency_invariant` and
  `boundary_pacing_does_not_regenerate`.
- The three streaming budgets (jobs / drain / uploads) summed to ~1.5×
  the nominal budget; they now consume one shared deadline, each keeping
  a minimum-progress floor.
- Uploads are bounded by BYTES as well as time (768 KiB on web) —
  `write_buffer` only records the copy, and Chrome's GPU process performs
  the staging transfer at submit, so the real cost landed after everything
  `upload_ms` could see.
- Eviction and the inbound net-event stream are both spread across frames
  now (join sync used to apply a server's whole journal in one frame).
- Persistence is sharded by 256×256-block region, so a save rewrites what
  changed rather than the world's entire edit history — see below.

### CSV schema changed

New columns: `dropped`, `refresh_ms`, `busy_ms`, `net_ms`, `sim_ms`,
`sav_ms`, `prep_ms`, `upl_q`, `evict_q`. As with the `gpu_water_ms`
insertion, **don't naively diff old and new CSVs**. `prep_ms` closes a
real attribution hole: remote sampling, name tags, UI rebuild and frame
uniforms were previously untimed, so a spike there appeared in
`total_ms` and in no phase at all. `DIG_SPIKELOG` is now an engine config
flag (`?spikelog` on web) rather than an env read that wasm always
answered None to.

## 2026-08-01 (seventh pass): water realism (lighting, falls, depth shading, spray, SSR)

Water gained fresnel + distance-widened specular + procedural sky
reflection on analytic ripple normals, two-phase texture advection,
per-vertex fall distance driving gravity stretch / aeration / streaks,
splash ring codes, and — via a render restructure — depth-based
shading: solids + sky now render to an offscreen scene color, the
solid depth is copied, and a separate water pass (blit composite +
prepass + color, new `gpu_water_ms` CSV column) samples both for
Beer-Lambert absorption, refraction, soft shores, and a desktop-gated
SSR march. Stateless spray billboards ride `MeshData.impacts`. The
platform-owned depth texture is gone (renderer-owned FrameTargets);
net GPU memory ≈ +1 full-screen color target.

Costs, ×3 runs at the shipping desktop config (SSR on):
gpu p50 main 1.78–1.90ms + water 0.58–0.89ms vs 2.28ms pre-change
single main pass — the split + blit + depth copy cost ≈ +0.1–0.4ms.
Frame pacing unchanged: avg 8.33–8.35, p95 9.3–9.5, hitches 1,
over-12.5ms 1–4 (sixth-pass profile). Mobile config (SSR off):
main 0.41–0.56 + water 0.45–0.49ms. SSR marches everywhere with a
distance-scaled stride (fixed 28 taps; far water takes coarse steps
that reach the mountains across a lake — an early 80-block cutoff
killed exactly those reflections); worst case (grazing ocean, ~60%
water coverage) ≈ +1.7ms, `--no-ssr` removes it.
Uncapped (`--no-vsync`): avg 2.77ms, 342 fps. Shader ALU additions
(ripples/fresnel/spec/reflection/advection, phases 1–2) measured
inside run noise. One caught regression: SSR ungated cost 1.70ms on
grazing ocean views — the distance gate more than halved it.

Reference shots: `shots/water-realism-shore.png` (soft shoreline,
absorption gradient), `shots/water-realism-sunset.png` (tod 75, yaw
−90: glitter path + sky reflection), `shots/water-realism-falls.png`
(coastal cascades: stretch, aeration, streaks, spray). Captures use
the new `--yaw`/`--pitch` flags (the spawn camera faces +z,
perpendicular to the sun's ±x azimuth plane). Note for future
pixel-diff verification: water shading is clock-animated and the clock
advances by real dt, so identical-flag runs differ on water pixels —
compare against a same-binary run-to-run noise floor, not byte
equality (this pass: ~37k px noise on water-heavy 1600×1200 views).
CSV schema change: `gpu_water_ms` inserted after `gpu_main_ms` — don't
naively diff old/new CSVs.

## 2026-07-31 (sixth pass): water system (levels, sloped surfaces, falls, sim, anim)

Water moved to Minecraft-style level blocks with its own two-word
vertex format + water.wgsl (solid path untouched), worldgen emits
WaterFall sheets at river pool steps (zero new noise — classify reuses
the neighbor fields it already had), and an edit-time fluid sim drains
128 queued cell updates per 0.25s world tick (idle = free). Terrain
retunes in between (ocean shelf + tanh continentalness + seafloor
detail, ridge^1.6 mountain profile) cost one tanh + one powf per
column. Costs vs the fifth pass: gen 1.03-1.06ms (flat), mesh avg
0.08 → 0.09ms, fill 4.66-4.81s (noise-level). One caught regression:
letting WaterState/emit_water_face inline into mesh_chunk's block loop
cost +0.03ms mesh avg on the dominant solid path — both are
#[inline(never)] now. Reference shots: `shots/water-falls.png` (seed
1234.5678 coastal cascade), `shots/sloped-water.png`.

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

## 2026-08-02 — two-word vertex format (Phase 2 format break)

All geometry moved to two u32 words/vertex (half-block positions for
future stairs, 10-bit texture layer): solid vertex bytes doubled. Bench
(vsync, seed 42): fill 4.56s, avg 8.35ms p99 10.68ms, over-12.5ms 3,
hitches 1, GPU buffers ~87-103MB during fill. Within noise of the prior
baseline — the doubled solid upload bytes did not move fill time or
frame pacing measurably. Deterministic capture is byte-identical to the
single-word format.

## 2026-08-03 — worldgen v5 retune (baseline drift note)

Fill-time baselines above predate the v5 terrain retune (region
wavelengths 2–3× longer, flat steppes): the same seed now generates
different terrain, so fill numbers will drift. Re-baseline on the next
bench pass; frame-pacing methodology is unchanged.

## 2026-08-05 — twelfths vertex repack (sub-block shapes)

Positions moved to 8-bit twelfth-block units (word0) with explicit
sub-rect UVs + tint in word1; shaders decode via an integer whole/
remainder split so full-block geometry is exact under GPU fast-math
(pixel-compared against the pre-repack build: differences at the same-
build run-to-run noise floor, ~30 bytes of 7.7M). Bench (vsync, seed
42): fill 4.51s, avg 8.39ms p95 9.48 p99 10.46, over-12.5ms 6, hitches
0 — within noise of the two-word baseline. Mesh jobs read 0.10ms avg
vs the ancient 0.04ms table entry; that row predates several format
changes and per-call state masking in `MeshInput::get` — throughput is
still ~5400 jobs/s, far above streaming demand. Shaped blocks mesh via
baked quad templates (`emit_shaped`, outlined like the water path), so
the hot full-block loop is unchanged.
