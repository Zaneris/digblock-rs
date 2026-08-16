# Movement / positioning audit — 2026-08-06

> **Status: F1, F2, F3 implemented** (see *Implementation* at the end). F4 and F6 are documented-only by design; F5 is
> not done.

Follow-up to `PERF-AUDIT.md`. Question asked: is the client/server positioning enforcement causing the walking stutter,
and if so is the adoption envelope too tight or is the correction smoothing too abrupt?

**Verdict: neither. The netcode is not the stutter.** The adoption envelope is doing its job with >2x headroom to spare,
corrections are rare (<1/s) and small (~0.07 blocks), and loosening the correction decay changes the measured smoothness
by 0.000. The stutter is a *rendering* bug that multiplayer merely amplifies: **the local camera samples the fixed 60 Hz
player sim with no interpolation**, so every millisecond of frame-pacing jitter becomes a millisecond of position
jitter.

Remote players — which *are* interpolated (`net/remote.rs`) — measure 100x smoother than the local camera under
identical conditions. That contrast is the whole finding in one line.

## Method

`scratchpad/netsim` is a closed-loop harness:

- Client half: the **real** `digblock_net::predict::Predictor` plus a transcription of engine.rs's per-frame block
  (`tick_accum` accumulator, `reconcile`, `send_inputs` stride, `correction_offset` decay,
  `camera.pos = player.pos +
  correction_offset`).
- Server half: a **verbatim copy** of `apps/server/src/sim/movement.rs` `step_client` + the `step_physics` drain policy,
  against the real `Player::tick` and the real `overlaps_solid`.
- Wire: per-datagram one-way latency + jitter + loss, snapshots at `SNAPSHOT_DIVISOR`, inputs at `INPUT_SEND_STRIDE`.
- A second binary drives the **real** `RemotePlayers` the same way.

Metric — `speedCV`: per rendered frame, apparent camera speed (`|Δcamera| / dt`) as a ratio of true walk speed; reported
as RMS deviation from 1.0. A perfectly smooth walk is 0.000. `min`/`max` are the worst single frames — **`min 0.00`
means frames where the camera did not move at all**, which is exactly what judder looks like. `bad%` is frames off by
more than 25%.

30 s walks, fixed seed. Numbers reproduce with `cargo run --release --bin netsim`.

## What the numbers say

### The network adds essentially nothing

| scenario                              | speedCV | min  | max  | bad%  | replay/s | meanCorr | srvGap |
| ------------------------------------- | ------- | ---- | ---- | ----- | -------- | -------- | ------ |
| offline, perfect 60 Hz pacing         | 0.000   | 1.00 | 1.00 | 0.0%  | –        | –        | –      |
| offline, ±3 ms pacing jitter          | 0.305   | 0.00 | 1.95 | 36.2% | –        | –        | –      |
| online localhost, perfect pacing      | 0.000   | 1.00 | 1.00 | 0.0%  | 0.0      | –        | 0.133  |
| online localhost, ±3 ms pacing jitter | 0.305   | 0.00 | 1.90 | 37.3% | 0.0      | –        | 0.141  |
| online 30 ms RTT, ±3 ms pacing jitter | 0.305   | 0.00 | 1.90 | 37.3% | 0.0      | –        | 0.211  |
| online 80 ms RTT ±10 ms net jitter    | 0.306   | 0.00 | 1.91 | 36.2% | 0.4      | 0.066    | 0.330  |
| online 80 ms RTT, 2% loss             | 0.331   | 0.00 | 1.96 | 37.9% | 0.6      | 0.070    | 0.330  |

Holding frame pacing constant, going from offline to 80 ms RTT with 2% packet loss moves speedCV from 0.305 to 0.331.
Going from perfect pacing to ±3 ms pacing jitter — *with the network unchanged* — moves it from 0.000 to 0.305. The
pacing term is ~40x the network term.

`srvGap` (mean |server pos − client pos|) sits at 0.13–0.33 blocks against `POS_TOLERANCE`. The envelope is not tight.

### It gets worse as the display rate diverges from 60 Hz

| scenario                       | speedCV | min  | bad%       |
| ------------------------------ | ------- | ---- | ---------- |
| 144 Hz display, ±0.5 ms jitter | 1.124   | 0.00 | **100.0%** |
| 60 Hz display, ±3 ms jitter    | 0.305   | 0.00 | 36.2%      |
| 30 Hz display, ±4 ms jitter    | 0.185   | 0.51 | 16.1%      |

At 144 Hz every frame is wrong, because 60 Hz of sim spread over 144 Hz of display is a 0,1,0,1,1,0,… pattern — over
half the frames draw the player in exactly the same place as the previous one. This is the mechanism behind "it feels
worse on my high-refresh monitor".

### Why multiplayer feels worse than local play

Same mechanism, different incidence. Multiplayer frames do strictly more work (snapshot decode, `pump_net` event
application, remote sampling, name tags), so they pace worse — and the un-interpolated camera converts pacing jitter
one-for-one into position jitter:

| scenario                                   | speedCV | bad%      |
| ------------------------------------------ | ------- | --------- |
| LOCAL: ±1.5 ms pacing, no hitches          | 0.266   | **10.7%** |
| MULTI: ±4 ms pacing, 4% hitches, 40 ms RTT | 0.331   | **44.0%** |
| MULTI + render interpolation               | 0.000   | 0.0%      |

4x the bad frames, with the netcode contributing ~0.02 of the 0.065 speedCV difference. The stutter is *correlated* with
multiplayer and *caused* by frame pacing.

### Both original hypotheses tested and rejected

| fix applied to the worst realistic case (80 ms RTT, jittery) | speedCV   | bad%     |
| ------------------------------------------------------------ | --------- | -------- |
| as shipped                                                   | 0.310     | 34.7%    |
| looser correction smoothing (tau 0.2 → 0.35 s)               | 0.310     | 34.4%    |
| **render interpolation**                                     | **0.024** | **0.0%** |
| both                                                         | 0.019     | 0.0%     |
| 144 Hz + render interpolation                                | 0.000     | 0.0%     |
| 30 Hz web frame + render interpolation                       | 0.015     | 0.0%     |

"Not enough fudge factor" — the envelope already has 2x headroom; replays fire 0–0.8 times per second during a steady
walk and the mean correction is 0.066 blocks, one tick of walking. Raising `POS_TOLERANCE` would buy nothing and cost
cheat resistance.

"Corrections too abrupt" — a 0.066-block offset decaying over 200 ms adds 0.33 blocks/s to camera velocity for a fifth
of a second, under 8% of walk speed, less than once per second. Below the noise floor of the aliasing. Measured effect
of relaxing it: **zero**.

### Remote players are already correct

Driving the real `RemotePlayers` with 20 Hz snapshots:

| scenario                              | speedCV | min      | bad% | lag (blocks) |
| ------------------------------------- | ------- | -------- | ---- | ------------ |
| 30 ms RTT, no jitter                  | 0.000   | 1.00     | 0.0% | 0.400        |
| 30 ms RTT, ±3 ms net jitter           | 0.001   | 1.00     | 0.0% | 0.374        |
| 80 ms RTT, ±10 ms net jitter          | 0.003   | 1.00     | 0.0% | 0.452        |
| 80 ms RTT, ±10 ms jitter, 2% loss     | 0.004   | 1.00     | 0.0% | 0.450        |
| 30 ms RTT, client clock 0.1% fast     | 0.001   | 1.00     | 0.0% | 0.260        |
| 30 ms RTT, client clock **0.5% fast** | 0.055   | **0.19** | 1.0% | **−0.217**   |

0.003 vs 0.310 for the local camera under the same network. The `INTERP_DELAY_TICKS` buffer plus the lerp in
`sample_ring` absorbs everything the network does. This is the proof that interpolation is the missing piece, not a
tuning problem.

The one weakness is at the bottom: `est_server_tick` is a **ratchet**
(`max(integrated wall clock, newest snapshot
tick)`) with no downward correction, so a client clock running fast drifts
the sample target past the newest snapshot into extrapolation, where the `EXTRAP_CAP_TICKS` hold produces frozen frames
(min ratio 0.19) and the drawn remote ends up *ahead* of truth (lag −0.217). 0.1% skew is fine; 0.5% is not. Not today's
stutter, but it is a latent one-way failure.

## Findings

### F1 — No render interpolation of the local player (the stutter)

`engine.rs:2806-2825`. `camera.pos = player.pos + correction_offset`, where `player.pos` is the raw post-tick state of a
fixed 60 Hz accumulator sampled at an unrelated display rate. Classic fixed-timestep aliasing — Fiedler, *Fix Your
Timestep!* (2004) is the canonical statement; every mainstream engine ships the fix (Unity `Rigidbody.interpolation`,
Unreal `ENetworkSmoothingMode` on `CharacterMovementComponent`, Source's entity interpolation per Bernier, GDC 2001).

Fix: keep the pre-tick position and render `prev.lerp(curr, tick_accum / TICK)`.

Costs and constraints:

- Adds half a tick (8.3 ms) of average visual latency. Standard and accepted; the alternative (extrapolating forward)
  overshoots into walls, which is much worse in a voxel game.
- The replay path must carry the anchor: `reconcile` moves `player.pos` by `−error`, so `prev` must move with it or the
  lerp re-injects the jump `correction_offset` exists to hide. (Verified in the harness — without it the fix
  under-performs.)
- `camera.pos` is *also* the raycast origin for dig/place (`update_target`, `apply_place`) and the argument to
  `placement_blocked`. Gameplay should keep using the true `player.pos`; only the view matrix should use the
  interpolated one. Sharing it would put a ≤0.067-block wobble into targeting — harmless against
  `EDIT_POS_TOLERANCE =
  1.5`, but there is no reason to introduce it.

### F2 — `placement_blocked` / raycasts run from the offset camera

Pre-existing, unrelated to F1 but found next to it. During a correction the camera is up to `POS_TOLERANCE` (0.75
blocks) away from the real body, so the "can't place a block inside yourself" check is evaluated at the wrong place — it
can permit a placement that clips the true capsule, or reject a legal one. The server re-derives from its own state so
this is a prediction-mismatch nuisance, not an exploit. Fix falls out of F1: pass `player.pos` to the occupancy checks.

### F3 — Snapshot acks are head-of-line blocked behind parked edits

`engine.rs:1218-1229`. `pump_net` drains one FIFO holding *all* net events and parks the entire remaining queue when
`NET_EDIT_BUDGET` (256) is exhausted. Edit→Layout ordering genuinely requires that, but `NetEvent::Ack` has no ordering
dependency on edits and gets stuck behind them.

Measured cliff:

| ack stall            | speedCV | replay/s | maxCorr  | srvGap    |
| -------------------- | ------- | -------- | -------- | --------- |
| 200 ms (fluid burst) | 0.305   | 0.0      | –        | 0.211     |
| 2.5 s (join sync)    | 0.319   | **18.3** | **1.60** | **1.458** |

A 200 ms stall is completely absorbed — the frozen-claim design in `predict.rs` means a late ack still matches what we
sent, so it costs nothing. That is a genuinely good property.

But past `BUF_CAP` ticks of unacked input, the acked tick has fallen out of the buffer, `predicted` is `None`, and
*every* snapshot takes the forced-replay path: 18 replays/s with 1.6-block corrections. At 256 edits/frame and 60 fps,
2.13 s of stall is ~15k edits — reachable at join on a long-running server, and `NetClient::ready()` is true from
`Welcome` onward, so the predictor is running while the sync drains.

Fix: dispatch `Ack` (and `Disconnected`) ahead of the parked queue, or give edits their own queue. Cheap and removes the
cliff entirely.

### F4 — Unloaded feet column freezes the input stream

`engine.rs:2786-2790`. When the column under the player has not generated, the engine zeroes `tick_accum` and the
`_ if
!column_loaded` arm matches before the net arm, so `send_inputs()` is skipped too. The server then underruns and
coasts on `last_intent` — it keeps walking while the client is frozen. Self-healing via adoption (measured srvGap
actually *drops* to 0.177), so this is not a correction source, but it is a hard visual freeze and it is much more
likely on web, where chunk jobs run inline on the main thread. Worth a note in whatever fixes F1: interpolation will not
hide a frame where the sim genuinely did not run.

### F5 — `est_server_tick` is a one-way ratchet

`remote.rs:45-59`. See the table above. Standard fix is a phase-locked estimator: track the observed offset between the
local clock and snapshot ticks and slew toward it, rather than `max()`. The adaptive jitter-buffer literature from VoIP
is the same problem. Low priority — 0.1% skew is harmless and 0.5% is unusual — but it can only ever fail in one
direction, which makes it a latent bug rather than a tuning choice.

### F6 — Input ticks are dropped by frames longer than ~67 ms

`predict.rs:117-118`. `input_batch` sends the newest `INPUT_REDUNDANCY` (4) unacked ticks. In steady state that always
covers the new ticks plus redundancy. But a frame that produces more than 4 ticks (>67 ms; `MAX_DT` caps a frame at 100
ms = 6 ticks) leaves the oldest permanently unsent. Consequence is bounded — the server runs fewer physics ticks than
the client and the next adopted claim pulls it forward 0.13 blocks — so this is a note, not a bug. Recording it so it is
not rediscovered as a mystery.

## What is healthy — do not "fix" these

- **The adoption envelope.** 0.13–0.33 blocks of steady-state gap against a 0.75 tolerance, 0–0.8 replays/s, 0.066-block
  mean correction. Widening it buys nothing and costs cheat resistance.
- **Frozen sent claims** (`BufEntry::sent`). This is what makes a 200 ms ack stall free, and it is why 30 ms and 80 ms
  RTT produce identical smoothness.
- **The 200 ms correction decay.** Measured effect of changing it: none.
- **Remote interpolation.** speedCV 0.003 under 80 ms RTT with 2% loss.
- **The drift EWMA.** Never fired in any honest scenario.

## Recommended order

1. **F1** — render interpolation. This is the stutter. 0.310 → 0.024, and 0.000 at 144 Hz.
2. **F3** — un-block acks. Removes a real cliff at join, small change.
3. **F2** — occupancy checks from `player.pos`. Falls out of F1.
4. **F5** — slewed remote clock. Latent, one-way, cheap.
5. **F4 / F6** — documented, no action proposed.

## Implementation

### F1 — `smoothing.rs`

New pure module `crates/digblock-render/src/smoothing.rs`: `TickLerp` holds the pre-tick position and exposes
`before_tick` / `rewind` / `reset` / `render_pos`. Six unit tests pin the behaviour headlessly, including one that
asserts the *bug* still reproduces without the fix (raw sampling stalls then doubles) so the test can't pass vacuously.

Wired into `engine.rs`'s walk branch: `before_tick` inside both sim loops (predicted and offline), `rewind(error)` on
`Reconciled::Replayed`, `reset` at every place that hard-sets the player (world swap via `reset_world_transients`,
`apply_teleport`, fly→walk re-entry, and the unloaded-column skip). The camera line becomes

```rust
self.camera.pos = self.tick_lerp.render_pos(self.player.pos, self.tick_accum)
    + self.correction_offset;
```

Three traps worth remembering, all of them found by building the thing rather than by reading it:

- **The rewind anchor is load-bearing.** `reconcile` moves `player.pos` by `−error` and the camera adds `error` back as
  `correction_offset`. Without moving the anchor too, the lerp spans the pre-rewind position and re-injects exactly the
  jump the offset exists to hide. Pinned by `rewind_carries_the_anchor` plus `rewind_is_load_bearing`.
- **The unloaded-column skip must re-anchor.** That path zeroes `tick_accum`, which parks alpha at 0 — drawing a full
  tick *backwards*. Pinned by `reset_prevents_backward_step_on_skipped_sim`.
- **Warm-up is not steady state.** The anchor starts equal to `curr`, so the first few frames legitimately lag the
  `render = v·(t − TICK)` identity. The smoothness assertion measures after frame 10.

### F2 — `body_pos()`

New accessor: `player.pos` while walking, `camera.pos` in fly (fly leaves `player.pos` stale, so the naive "just use
player.pos" swap would have checked a position from wherever the player last walked). Both `placement_blocked` call
sites now use it.

Aim rays deliberately still originate at `camera.pos` — you hit what you see, and `EDIT_POS_TOLERANCE` (1.5) is
explicitly sized for a camera carrying the reconciliation offset. Interpolation adds at most 0.067 blocks to that
budget. `eye_in_water` likewise stays on `camera.pos`; its `FLIP_EARLY` hysteresis (0.1) already exceeds the
interpolation lag.

### F3 — acks bypass the backlog

`pump_net` now partitions incoming events as it merges them into the backlog: `NetEvent::Ack` goes straight to
`self.pending_ack`, everything else keeps its FIFO position. The dead arm in the dispatch match is kept as a backstop
rather than deleted, so a future queued ack still lands instead of being silently dropped.

Ordering check: if the first snapshot beats `Welcome` out of the queue, the ack is set and then cleared by `swap_world`
→ `reset_world_transients`, costing one snapshot interval on the first reconcile. Correct, and the stale-world hazard
runs the safe direction.

### Not done

- **F5** (`est_server_tick` ratchet). Latent, needs a slewed/PLL clock and a way to test clock skew end to end;
  deliberately left.
- **F4 / F6** were findings-only from the start.

### Verification

`just check` (fmt, clippy `-D warnings`, `cargo test --workspace` including the Docker-backed server e2e suite),
`just
check-web`, and `just check-ios` all pass. A 240-frame screenshot run confirms the render path is unchanged.

What is *not* verified here: how it feels. The harness and the unit tests both measure a model of the frame loop, not
the real one — the half-tick (8.3 ms) of added visual latency in particular is a judgement call that only shows up in
play. Worth checking on the web build specifically, where frames are longest and the payoff should be biggest.
