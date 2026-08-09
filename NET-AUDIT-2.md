# Netcode audit — performance, validation, and the master trust model

Date: 2026-08-08. Scope: `digblock-net`, `digblock-net-native`, `digblock-net-web`,
`apps/server`, `apps/master`, and the net-facing parts of `digblock-render`
and the two service fillers.

Companion to `NET-AUDIT.md` (movement/positioning, 2026-08-06) and
`PERF-AUDIT.md` (frame consistency). Those two are closed; this one is the
record for the round that followed. Findings that were fixed in that round
live in the git history — **this file is the standing list of what was
deliberately NOT fixed, and why.**

The short version: the netcode's *mechanisms* are sound. The per-tick
budgets, the interest management, the append-only wire discipline, the
adoption envelope, the predict/confirm/reject precedence — all of it holds
up under adversarial reading. What does not hold up is the **master's trust
model**, which has no notion of an account. Everything in §1 is one bug
wearing seven hats.

---

## 1. Blocked on the master login flow

**None of these are fixable at the game server.** They all reduce to the
same missing primitive: the master mints an identity for anybody who asks
for one. `POST /api/join` (`apps/master/Program.cs:155-167`) takes a
`displayName`, checks it against `^[A-Za-z0-9_-]{3,10}$`, and hands back a
valid token. There is no account, no ownership, no reservation, and no rate
limit. Likewise `POST /api/register` (`Program.cs:64-104`) has no credential
at all — the QUIC probe is a *reachability* check that everyone mistakes for
an *authentication* check.

Deferred until a real login flow exists. Revisit **all seven together** when
it lands; fixing any one in isolation just moves the hole.

### 1.1 Admin authority is a self-asserted display name — CRITICAL

`apps/server/src/sim/mod.rs:516-519` (and the join-time copy at `:969-972`)

```rust
let is_admin = self.admin_name.as_deref()
    .is_some_and(|a| client.name.eq_ignore_ascii_case(a));
```

`client.name` is whatever the master returned from `/api/tokens/verify`
(`apps/server/src/net.rs:251-257`), and the master will return whatever the
joiner asked for. The admin name is public — it shows in the roster, in chat,
in name tags, and `just play` hardcodes `admin`. So:

```
curl -XPOST $master/api/join -d '{"serverId":"<public>","displayName":"admin"}'
```

is a complete server takeover: `/ban`, `/tp`, `/bring`, `/set-spawn`,
`/set-spawn-dir`, `/lock-spawn`, `/time`.

Two things make it worse. Nothing enforces name uniqueness at join —
`mod.rs:844-850` refuses a duplicate *identity* hash, not a duplicate name —
so the impersonator can be admin **while the real admin is online**. And the
impersonator can `/ban` the real admin (one live name match, different IP, so
the shared-IP guard at `admin.rs:534` passes), while the real admin cannot
`/ban` back: two live matches for the name hits the ambiguity branch and
refuses.

The gate itself is structurally correct — there is exactly one call site of
`handle_admin_command` (`mod.rs:537`), so a command added to the dispatch
table inherits it. It is keyed on the wrong thing, and that is the whole bug.

**Fix when login lands:** key admin off the authenticated account id the
master vouches for, never off the display string. Consider also refusing a
join whose display name is already live on the server.

### 1.2 Server registration is unauthenticated and evicts the incumbent — CRITICAL

`apps/master/Program.cs:64-104`, `apps/master/Registry.cs:89-92`

Registering the same `host:port` silently replaces the existing entry, taking
its id and bearer key with it. The only gate is the QUIC probe, which merely
proves that *something* at that address serves a certificate matching the
submitted hash — and that hash is published to the world at
`GET /api/servers` (`ServerSummary.CertHash`).

So any unauthenticated party reads a victim's `url` + `certHash`, POSTs
`register` with those exact values, and passes the probe **because the probe
dials the victim**. The victim's `ServerEntry` and key are destroyed; its next
`/api/tokens/verify` gets `FindByKey → null → 401 → VerifyError::Denied`, and
every join is refused until it re-registers. The victim re-registers on its
30 s heartbeat 404, so a registration loop keeps a server permanently
unjoinable. Meanwhile the attacker controls the listed `Name` for that address
and holds a valid server key.

`apps/server/tests/e2e.rs:537-541` exercises this exact primitive — re-register
the same host:port under a different name with no credential — as a *feature*.
That test will need to change.

**Fix when login lands:** registration needs an operator credential, and a
re-registration must present the existing entry's key.

### 1.3 `probeHost` is client-supplied and is the only address ever dialed — CRITICAL

`apps/master/Program.cs:93-97`, `apps/master/HealthChecker.cs:51`

`probe.ProbeAsync(probeHost, ...)` and the health sweep both dial
`entry.ProbeHost`, while the `entry.Url` handed to clients
(`Registry.cs:36`) is built from `Host`. Register `probeHost` = your own
working QUIC endpoint and `host` = any third-party address, and the master
lists — and indefinitely keeps alive — an entry pointing at an address it has
never verified, aiming every client that picks that row at the victim.

This voids precisely the guarantee `Program.cs:25-38` refuses to boot without.
CLAUDE.md notes that production servers omit `probeHost`, but nothing in the
master requires or constrains that.

**Fix when login lands:** probe the advertised `Host`. If a dev-only NAT
hairpin escape is still needed (the `host.docker.internal` case), gate it
behind an authenticated operator flag, not an anonymous request field.

### 1.4 The identity secret is a global bearer credential — HIGH

`crates/digblock-net/src/messages.rs:130-133`, `apps/server/src/sim/mod.rs:844`

`Hello.identity` is one 16-byte secret, loaded from a single file
(`apps/desktop/src/main.rs:266-297`) or one localStorage key
(`apps/web/src/lib.rs:130`), and replayed verbatim to **every** server the
player joins. Server-side it is the sole key to the `PlayerRecord` —
inventory, layout, last position — with no binding to the master-verified
display name (`hash_identity`, `player_store.rs:50`).

Chained with 1.2: anyone can list a server under any name, a player joins it,
and that operator now holds the player's secret and can present it on the real
server to inherit their inventory and position. The server detects only the
*concurrent*-connection case (`sim/mod.rs:845-849`), not a later replay.

The file-mode half of this (0644 → 0600) was fixed in this round. The design
half is deferred: the secret should be scoped per-server, or replaced entirely
by the account the master vouches for.

### 1.5 Persisted records can be evicted by anyone — HIGH

`apps/server/src/player_store.rs:43,145-154`

`MAX_IDENTITIES = 10_000`. The LRU-order bug (re-`put` never refreshing a
key's position, so online players were evicted before idle ones) was fixed in
this round. The root cause was not: `identity` is client-chosen and
`/api/join` mints tokens for free at any rate, so 10,000 join/disconnect
cycles still evict every legitimate record, oldest first.

**Fix when login lands:** key records off the authenticated account, and let
the account system rate-limit account creation.

### 1.6 Ban enforcement runs after the master round trip — MEDIUM

`apps/server/src/net.rs:251` vs `apps/server/src/sim/mod.rs:808-812`

`verify_join` — a blocking HTTP call to the master that **consumes the
one-time token** — completes before `SimEvent::Connected`/`Hello` ever reach
`handle_hello`, which is where `is_banned` is finally consulted. With
`MAX_CONNECTIONS = 512` and no per-IP connection cap, a banned peer (or any
peer) can hold 512 concurrent sessions and force 512 outbound verifies per
`VERIFY_TIMEOUT` window, turning the game server into an unauthenticated
amplifier against its own master.

Partially mitigated in this round by bounding the pre-Hello lifetime, which
caps how long a slot can be squatted. The ordering itself is deferred: moving
the ban check earlier means the net layer needs the ban list, and the clean
version of that is to ban an *account*, not an IP.

Note also that bans are IP-only (`ban_store.rs:47`) — deliberate, but it means
evasion is a new IP while the evader's inventory and position survive intact,
since those are keyed by identity. With an account system, ban the account.

### 1.7 Unauthenticated outbound probe with an error oracle — MEDIUM (SSRF)

`apps/master/Program.cs:97`, `apps/master/Probe.cs:44-106`

`RegisterFailed(Reachable, Reason, Detail)` is returned to the caller with
`Reason` ∈ {`dns`, `timeout`, `handshake`, `certMismatch`} plus the raw
exception message. That distinguishes "nothing there" from "something
completed a QUIC/TLS handshake" from "DNS failed", for any `host:port` the
master can reach — including the Azure Container Apps internal network it is
deployed into. `Dns.GetHostAddressesAsync` is called on the supplied string
with no allowlist and no private-range filter.

**Fix when login lands:** authenticate registration (which removes the
anonymous oracle), and separately collapse the failure detail to a single
opaque reason.

---

## 2. Deferred for other reasons

### 2.1 Snapshot bandwidth quantization — DONE (protocol v14)

Landed after this document was first written; kept here because the
reasoning is the record of why the wire looks the way it does.

`NetPlayerState` was one shape serving two jobs, at 40 wire bytes per
player. Of those, ~16 were provably discarded:

- `look: [f32;3]` (12 B). The only consumer was `remote.rs` —
  `look.x.atan2(look.z)`. **The pitch component was never read anywhere**,
  and the local player's own entry discarded `look` entirely.
- `vel: [f32;3]` (12 B), used only for extrapolation capped at
  `EXTRAP_CAP_TICKS` (250 ms).

`pos` had to stay `f32`: `POS_EPS2 = 1e-6` (`predict.rs`) means the echoed
self position must round-trip bit-comparably or every single snapshot takes
the replay path. So the fix was a **split**, not a shrink —
`SelfState` (full-precision pos/vel, no look, no id) beside `RemoteState`
(f32 pos, `i16` velocity in thousandths, `u16` yaw, held).

Two things worth remembering:

- **Yaw is interpolated in `u16` turn space, not in radians.**
  `b.wrapping_sub(a) as i16` IS the signed shortest arc, wrap included. The
  old code only avoided the long-way-round spin at the discontinuity
  because it lerped the look VECTOR and renormalized; lerping decoded
  radians instead would have introduced the bug.
- **`MAX_SNAPSHOT_PLAYERS` is sized for the WORST-CASE encoding, not the
  typical one.** postcard varints an `i16`/`u16` to 3 bytes at its
  extremes, not 2 — the first cut of this assumed 2, and the cap it
  produced was one an all-extremes snapshot would have overflowed. An
  oversize snapshot does not truncate; it fails to encode and that client
  gets nothing, which is the exact cliff interest management exists to
  close. `a_full_snapshot_fits_one_datagram` now pins the encoder against
  the arithmetic.

Measured against reconstructed v13 shapes, a realistic walking crowd
(4 blocks/s, ordinary held ids):

| players | v13 | v14 | | @20 Hz |
|---|---|---|---|---|
| 2 | 84 B | 55 B | −35% | 1.7 → 1.1 kB/s |
| 8 | 312 B | 187 B | −40% | 6.2 → 3.7 kB/s |
| 16 | 616 B | 363 B | −41% | 12.3 → 7.3 kB/s |
| 24 | 920 B | 539 B | −41% | 18.4 → 10.8 kB/s |

`MAX_SNAPSHOT_PLAYERS` rose 24 → 40 inside the same datagram.

**The follow-up lever, also now landed — as tiering, not a smaller
radius.** The snapshot selection radius is `client.draw_dist` = 20 columns
= 320 blocks, and the first version of this note claimed a player is
"sub-pixel" out there. That was wrong, and worth recording because it very
nearly justified the wrong fix: at 320 blocks a 1.8-block player subtends
~5 px at 1080p and 70° vertical FOV (~12 px at 128 blocks). Small, but
plainly visible — cutting the radius would have deleted people you can
see, to save bytes.

So distance tiers the RATE instead. Inside `PLAYER_NEAR_COLS` (8 columns,
128 blocks) a remote rides every snapshot; beyond it, one in
`FAR_PLAYER_STRIDE` (4) — 5 Hz. Which snapshot is phased by player id, so
far players spread across the cycle rather than all landing in one
datagram and then none.

Cost: the client interpolates ~100 ms behind and extrapolates along
velocity to a 250 ms cap, so far players run mostly on extrapolation —
worth a few tenths of a block at walking speed, on a figure a handful of
pixels tall. `REMOTE_EXPIRE_TICKS` (2 s) sits far outside the 200 ms send
interval, which a `const_assert` now pins so the tiering can never expire
the players it is merely sending slowly.

Benefit, for uniform density in the disc: only 16% of players in range are
inside the near radius, so a snapshot carries ~37% of the remote entries
it used to — **~63% fewer**, on top of v14's ~41% fewer bytes each.

### 2.2 Join-sync pacing and the PlayerStore fsync — DONE

Both landed after this document was written; recorded because the shapes
are load-bearing.

**Join sync is queued, not sent.** `handle_hello` used to encode every
in-range edited column and push it in ONE tick — thousands of frames and
megabytes inside a 16.7 ms budget on a mature world, a hitch for everyone
else on the server every time anyone joined, and past `OUT_CAP` a race the
joiner simply lost (it was dropped for "overflowing the outbound queue"
through no fault of its own). Columns now go into a per-client queue that
`pump_join_sync` drains a few per tick, and only into outbound slots that
live traffic is not going to need. That reserve is what makes the cliff
structurally impossible rather than merely unlikely. `JournalDone` moved
ahead of the columns — the client ignores the message, and nothing after
it depends on them — so the handshake completes immediately and terrain
fills in over the next few ticks.

**The scan is region-indexed.** `JournalIndex::columns_near` walks only the
regions a disc can touch. The old `edited_columns` walk ran per client
every time their center column moved — every 16 blocks — and grew for the
life of the world while every other cost around it was O(active). It
returns a superset (whole regions); callers still apply the radius test,
and `columns_near_never_misses_what_a_full_scan_finds` pins that it can
never return LESS than a brute-force filter.

**Decoding is budgeted client-side too.** `NetClient::poll` used to decode
the entire arrived backlog in one call. A `JournalColumn` expands to one
event PER CELL, so a few hundred columns is six figures of events
materialised in a single rAF callback on wasm — the reason the budget is
in events, not frames, and why it matches the engine's own per-frame apply
budget so the backlog cannot outrun its drain.

**The PlayerStore fsync moved off the sim thread.** A flush rebuilds the
whole file and fsyncs it: 1-10 ms local, 20-100 ms on a container volume,
i.e. several dropped ticks every interval. Encoding stays on the sim
thread (it needs the live map); the write does not. Submissions coalesce
— one buffer in flight, a newer snapshot replaces an unstarted older one —
so falling behind costs staleness, never unbounded queueing, and the
newest state wins, which is the only thing a whole-file snapshot can
express anyway. A failed write re-arms `dirty` through a shared flag so
the next interval retries.

### 2.3 Liveness and the untestable disconnect path — DONE

Three related gaps, fixed together because the third is what tests the
other two.

**The loopback transport could not express a disconnect.** `status` was
hardcoded to `Connected` and `poll` never emitted `Disconnected`, so no
in-process test could exercise ANYTHING the client does after `Welcome`
when a connection dies — coverage stopped at the pre-Welcome handshake
timeout. It now has `close` (drain what is queued, then report) and
`go_silent` (deliver nothing, report nothing — a peer that hangs rather
than hangs up).

That paid for itself immediately: the first test written against it caught
a live regression from the join-sync work in §2.2. Budgeting the decode
had moved it AFTER the transport event loop, which silently inverted
disconnect ordering — the close was reported before the bytes that
preceded it, and since the engine clears its parked queue on a disconnect,
those events were being thrown away instead of applied. `poll` now drains
frames when it encounters the disconnect, before pushing it, under one
budget shared across the whole call.

**The client had no post-handshake deadline.** `check_deadlines` returned
early once `ready()`, so a peer that stayed open but stopped answering was
never noticed. `RECV_TIMEOUT_SECS` (35 s) is a backstop, not the primary
mechanism — normally the transport reports the death itself. It is sized
off `TOD_SYNC_TICKS`: the server broadcasts time-of-day every 10 s to
every client, live or not, so even a player still waiting on their spawn
column has a heartbeat. A `const_assert` pins the relationship, and caught
the first value (30 s) sitting exactly ON the third heartbeat with no
slack for scheduling jitter.

**The native transport could not report a dead thread.**
`ChannelTransport::poll` used `try_iter`, which is silent on a dropped
sender, and `status` is a cached field — so a background thread that
panicked or unwound past its `disconnect` call was indistinguishable from
an idle one. It now distinguishes `TryRecvError::Disconnected` and
synthesizes the disconnect the thread never sent.

### 2.4 Client-direction frame cap — DONE

`MAX_FRAME` was 1 MB in both directions, sized for the server's journal
batches. The widest thing a client legitimately sends is a `Layout` at a
few hundred bytes, but that shared cap let one push 1 MB `Layout` frames
(a `Vec<u16>`, truncated only later during sanitization) straight into the
sim's action queue. `MSG_BUDGET` bounded the rate, so this was
amplification rather than a heap bomb — but a cap three orders of
magnitude past any real message is not a cap.

`FrameReader` now carries its own limit (`Default` keeps `MAX_FRAME` for
the client reading a server; the server uses
`with_limit(MAX_CLIENT_FRAME)`, 4 KB). The refusal happens on the length
PREFIX, before the body is buffered, which is the property that matters.
`max_client_frame_fits_every_client_message` builds each client message at
its widest — a name at `MAX_NAME_LEN`, a chat line at `MAX_CHAT_LEN` in
two-byte characters, a full hotbar+grid of three-byte varint ids — so the
cap cannot be tightened into dropping honest players.

### 2.5 Contract drift — the player-count half DONE

`Contracts.cs` types `Players`/`MaxPlayers` as `int` and range-checked
neither the register nor the heartbeat, while both Rust fillers decode
them as `u32`. Since the fillers deserialize the whole `ListResponse` in
one shot, ONE entry registered with `players: -1` failed the entire decode
— and no client could see ANY server on that master.

Fixed on both sides, because either alone leaves a gap: the master clamps
(`ServerRegistry.SanitizeCount`, applied at register AND heartbeat) so the
bad value never leaves, and the fillers clamp on the way in so a stale or
hostile master cannot do it either. A count is display text beside a name;
it was never worth the whole listing.

Pinned by an e2e test that registers by hand with `players: -1` — a
well-behaved client cannot send it, since `RegisterBody` is `u32` — and
asserts the listing is still readable. Verified to fail against the
unclamped master.

**Still open in this section** (all cheapest to do alongside §1, since
they are the same three-way mirror):

- `ProtocolVersion`/`SimVersion` (`Contracts.cs:17-18`) are sent by
  `master.rs` and never read by the master or surfaced in
  `ServerSummary`. Incompatible servers are listed indistinguishably from
  compatible ones, and the client discovers the mismatch only after a full
  QUIC connect + Hello.
- `JoinResponse.ExpiresIn` is read by neither filler.
- The server never re-validates the master's `displayName` with
  `validate_name` — only `sanitize_name` runs, which permits 24 chars
  including spaces. The 3-10 ASCII rule is enforced in exactly one place,
  with no defence in depth on the side that consumes it. That matters more
  than it looks, because §1.1 makes the name an authorization input.

### 2.6 The small ones — DONE

- **Command-vs-chat was decided on unsanitized text and executed on
  sanitized text.** They disagree for any character the sanitizer strips
  that `trim_start` does not count as whitespace — `U+200B` is exactly
  that: not `White_Space`, not inside `TEXT_RANGES`. So
  `"\u{200B}/ban griefer"` read as ordinary chat, sanitized to
  `/ban griefer`, and was BROADCAST to every player: a non-admin
  publishing a command, or an admin leaking one they meant to run. The
  handler now sanitizes FIRST and decides from the result, so both halves
  read the same string by construction rather than by agreement.

- **`--admin-name` was never validated.** Admin is granted by matching
  against a display name, which the master constrains to `validate_name`
  (3-10 chars of `a-z A-Z 0-9 - _`), so `--admin-name "big boss"` could
  never match anyone — the server booted with admin silently disabled
  while logging a reassuring `admin: big boss`. The launcher now exits on
  it, and `set_admin_name` refuses to store one, so neither entry point
  can leave a server in that state. (This validates the SHAPE only; names
  remain self-asserted, so admin stays impersonable until §1.1.)

- **`noop_catch` was dropped while write promises were still pending.**
  `close()` rejects every in-flight write, and those rejections are
  microtasks that fire after the wasm call returns — by which point a
  client-owned closure has been dropped with the struct, so wasm-bindgen
  threw "closure invoked after being dropped" from inside a catch handler
  on every disconnect. The handler is now module-lifetime
  (`thread_local!`), owned by nothing that can go away first.

---

## 4. Still open

Everything in §1, and the remainder of §2.5. Both wait on the master
login flow — §2.5's leftovers are the same three-way mirror, and its
display-name item in particular only becomes meaningful once a name is
something the master vouches for rather than something a joiner picks.

§4.1 has since been fixed and is kept here rather than moved, because the
two diagnoses it had to walk back are the useful part. What it leaves open
is server-side **aim** lag, called out at the end of it.

### 4.1 Input delivery under jitter — FIXED (`INPUT_HOLD_MAX`)

Input delivery used to fall to 91.8% at 300 ms/5% and 73.0% at 500 ms/10%.
Jitter caused it, not loss: heavy jitter reorders datagrams, and
`handle_inputs` skips anything at or below `last_input_tick`, a high-water
mark, so a tick arriving after a newer one had been consumed was discarded
rather than applied out of order. Redundancy could not help — every copy of
a tick is equally old.

An earlier draft blamed `JITTER_BUFFER_TICKS = 2` for being "too small to
absorb ±150 ms". **That was the wrong diagnosis**, and worth keeping on the
record. The constant is only the threshold above which the drain takes two
inputs in a tick; the drain always took at least one when the buffer was
non-empty and had no way to take zero, so depth was a one-way ratchet — the
`depth` column read ~1.5 ticks even on a clean LAN, and that 1.5 was entirely
`INPUT_SEND_STRIDE` burstiness rather than absorption. Raising the constant
would have changed nothing.

**The fix is `INPUT_HOLD_MAX` (`movement.rs::step_physics`):** the drain
holds a *hole* in the tick sequence open for up to 4 ticks, so the late input
is still admitted. Delivery goes to 98.0% and 89.5%. The lan and wifi rows
are unchanged to the digit — no reordering, no holes, no holds — which is
what makes this free for almost every player. Numbers, the cap sweep it was
chosen from, and the honest debits are in BENCH.md.

Two of the three caveats in the previous draft held up; one did not.

- **The adoption envelope already absorbed most of the positional damage.**
  Held. It is why this was polish and not an emergency.
- **What the envelope does not repair is `client.look`.** Still true, still
  unfixed, and the new `dig ok/att` column shows it: at 500 ms a hold can
  exceed `DIG_OFF_TARGET_RESET`'s 30 consecutive off-target ticks and be
  abandoned outright, while `dig short` stays zero — so it is aim lag, not
  `dig_grace`, that fails there. Aim has no adoption path. **This is the
  remaining open item on this path**, and the 8-attempt sample the bench
  currently gets is too small to size it.
- **"A deeper buffer cannot land without making `dig_grace` and
  `EDIT_POS_TOLERANCE` depth-aware."** **Wrong**, and the reason matters. The
  measured `orig` residual *falls* under a bounded hold (0.60 → 0.40 at
  300 ms, 1.02 → 0.87 at 500 ms), because the alternative to holding is
  coasting on stale intent and a coast diverges faster than a delay. Making
  those constants elastic would also have been the one version of this change
  with real cheat surface: depth is client-controlled through tick numbering,
  `verify_digs` casts `REACH + EDIT_POS_TOLERANCE`, and scaling the tolerance
  with an 18-tick depth would have bought ~1.2 blocks of extra reach for
  anyone willing to look like a bad connection. **Their fixedness is a
  standing constraint, not an oversight** — it is recorded as a rule on the
  constant.

The server-side counters that made this falsifiable (`Sim::input_stats` and
`Sim::edit_stats`) remain purely diagnostic — nothing reads them back into a
decision, including the hold, which keys only on whether the next tick is
present.
