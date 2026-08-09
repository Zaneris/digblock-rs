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

**Still open, and the bigger lever:** the snapshot interest radius is
`client.draw_dist` (`sync.rs`) = 20 columns = **320 blocks**, at which a
player cube is sub-pixel. A dedicated, smaller player-interest radius — or
a distance-tiered send rate — would cut typical P far more than any
encoding change, and needs no protocol change at all. Left alone here
because it changes what players can SEE, which is a design call rather than
an optimization.

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

### 2.3 No post-handshake liveness deadline on the client

`crates/digblock-net/src/client.rs:259-262` — `check_deadlines` returns
immediately once `ready()`. A server whose transport stays open but stops
replying is never detected client-side; detection is left entirely to the
QUIC/WebTransport idle timeout. Low severity (the idle timeout does fire), but
it means `PendingEdits` and the parked backlog grow unbounded in the interim.

Related: the native transport cannot *report* a dead background thread —
`ChannelTransport::poll` uses `try_iter()`, which is silent on a dropped
sender, and `status` is a cached field (`channel_transport.rs:98-107`). A
panic in the net thread leaves the engine in a session with no snapshots, no
error, and no way back. A `TryRecvError::Disconnected` check would close it.

And `loopback.rs:105-107` returns `Connected` unconditionally and never emits
`Disconnected`, so **no in-process test can exercise the post-Welcome
disconnect path at all**. `tests/loopback.rs` covers only the pre-Welcome
`handshake_timeout_disconnects`. Worth fixing before the two above, since it
is what would test them.

### 2.4 Client-direction `MAX_FRAME` is sized for the server's journal batches

`protocol.rs:242` — 1 MB in both directions. The largest legitimate
`ClientMsg` is a few hundred bytes, but a client can send 1 MB `Layout` frames
that are queued raw into `Sim::action_queue` before sanitization. `MSG_BUDGET`
(32 per 100 ms) bounds this to amplification rather than a heap bomb, so it is
low priority — but a separate, much smaller client-direction cap is the
missing guard.

### 2.5 Contract drift in the three-way mirror

CLAUDE.md requires `Contracts.cs`, `master.rs` and `services.rs` to move
together. Three disagreements, none urgent:

- `Contracts.cs:32` types `Players`/`MaxPlayers` as `int` with no range
  validation at `Program.cs:101`; both Rust fillers deserialize `u32`. One
  entry registered with `players: -1` fails the whole `ListResponse` decode,
  so **no client can see any server**. (Trivially reachable today via 1.2;
  once registration is authenticated it needs only a buggy server.)
- `ProtocolVersion`/`SimVersion` (`Contracts.cs:17-18`) are sent by
  `master.rs:180-181` and never read by the master or surfaced in
  `ServerSummary`. Incompatible servers are listed indistinguishably from
  compatible ones, and the client discovers the mismatch only after a full
  QUIC connect + Hello.
- `JoinResponse.ExpiresIn` (`Contracts.cs:38`) is read by neither filler.

Also: the server never re-validates the master's `displayName` with
`validate_name` — only `sanitize_name` runs, which permits 24 chars including
spaces. The 3-10 ASCII rule is enforced in exactly one place, with no defence
in depth on the side that consumes it. That matters more than it looks,
because 1.1 makes the name an authorization input.

### 2.6 Minor, noted but not scheduled

- **Command-vs-chat is decided on unsanitized text and executed on sanitized
  text** (`sim/mod.rs:515` vs `:530`). `U+200B` is not `White_Space` and not in
  `TEXT_RANGES`, so `"\u{200B}/ban griefer"` reads as ordinary chat, then
  sanitizes to `/ban griefer` and is **broadcast to every player**. An admin
  who pastes a command with an invisible prefix publishes it instead of
  running it. Both sides of the decision should read the same string.
- **`--admin-name` is never validated** against `validate_name`
  (`main.rs:93`), so `--admin-name "big boss"` silently grants the commands to
  nobody while logging a reassuring `admin: big boss` at boot.
- **Admin commands bypass the chat token bucket** (`mod.rs:523-528`,
  deliberate and test-pinned). `/time` is O(players) fan-out and `/ban` /
  `/set-spawn-dir` each fsync, so at the net layer's 320 frames/s ceiling
  they are a 320-fsync/s primitive on the sim thread. Only reachable by an
  admin — i.e. by anyone, until 1.1 is fixed.
- **Hostile `u32::MAX` tick values** permanently disable client machinery for
  the session: in `predict.rs:162` a `your_last_input_tick` of `u32::MAX`
  makes every later ack `StaleAck` (prediction never reconciles again); in
  `remote.rs:59` a `server_tick` of `u32::MAX` ratchets `est_server_tick` to
  4.29e9 and evicts every remote player forever. Neither crashes. Both need a
  hostile server, and both are subsumed by the clock-slew work `NET-AUDIT.md`
  deferred as F5.
- **`noop_catch` is dropped while write promises are pending**
  (`net-web/transport.rs:380-389`): `close()` rejects in-flight writes as
  microtasks that fire after the wasm call returns, so wasm-bindgen throws
  "closure invoked after being dropped" inside a catch handler. Console noise
  on every web disconnect — harmless, but it is exactly the noise that makes a
  real disconnect bug unreadable.
- **`refused_cell` survives world changes** (`engine.rs:345`).
  `reset_world_transients` clears every other net transient but not this one,
  and its only other clear fires while the dig button is held *and* aimed
  elsewhere. A cell refused by one server's spawn lock stays undiggable at the
  same coordinates in the next world until that happens to occur.

---

## 3. What was checked and found sound

Recorded so the next audit does not re-derive it.

**Wire and framing.** `FrameReader` handles arbitrary chunking; the length
prefix is bounds-checked before any allocation; `write_frame` restores the
buffer on failure; the frozen 8-byte preamble is correctly reassembled across
a chunk split on both sides. postcard's cautious size hint means no
length-prefix allocation bomb inside a frame. The golden-bytes tests in
`messages.rs` genuinely pin the append-only discipline.

**The server's trust boundary for edits.** Every client-supplied field is
validated: `origin`/`dir` finite-checked, normalized and tolerance-checked
against the server's own position; `cell` must equal the server's own
re-raycast hit; `normal` must equal `hit.normal`; `block` passes
`state_valid` plus the door and slab gates; `slot` bounds-checked before
indexing; `EditRequest.tick` deliberately ignored; alchemy ids through
`try_from_id` and counts through `1..=ALCHEMY_MAX_COUNT`; `Layout` vectors
sanitized then reconciled against authoritative counts; `InputTick`
finite-checked and normalized. The one gap found — the place target's
replaceability — was fixed in this round.

**The item economy.** No duplication path exists. Dig credit is taken once
from the first write's journaled `prev` (so door pairs credit once); the place
debit's only refund path refunds exactly one; `Inventory::add` saturates at
`cap()`, and water's cap of 1 means the bucket cannot be over-charged; the
slab economy closes exactly (9 sub-units × 3 = 27 = one block); there is no
craft→convert loop, because only Wood and Stone have a `sub_material()`, so an
18-unit stair cannot be converted back for 27. The `verify_digs` full-cell
comparison closes the grow-under-a-hold and swap-under-a-hold races, and wire
ordering closes craft-then-place and assign-then-place. All test-pinned.

**Predict/confirm/reject precedence.** `PendingEdits`' structural ordering is
correct, including the multi-cell partial-supersede case. Every `EditRequest`
gets a reply on every path — including the queue-cap flood, not-live, no-token
and unhealthy-journal cases — so pending entries cannot leak.

**Cancel-safety.** Both `select!` loops were checked against the vendored
quinn/wtransport sources rather than assumed: `RecvStream::read` and
`receive_datagram` are cancel-safe, and the one non-cancel-safe call
(`write_all`) sits in an arm body, never a branch future.

**Backpressure.** The server is properly bounded end to end: `OUT_CAP` with
`try_send` everywhere, refusal marks the client dead rather than buffering,
and inbound budgets *pause the read* so QUIC flow control backpressures the
sender. Nothing blocks the sim thread on a slow peer.

**Auth mechanics, as distinct from the trust model.** Token entropy (16 bytes
from `RandomNumberGenerator`) and one-time-ness (atomic `TryRemove`) are
correct; the local `used_tokens` TOCTOU is a latency backstop, not the guard.
Every master-failure branch fails closed — 200-with-unparseable-body, 5xx,
non-401/404, transport error, task panic and outer timeout all refuse. Bearer
keys compare with `FixedTimeEquals`. `inventories.bin` v1/v2/v3 migration is
bounds-checked throughout and cannot panic or over-allocate on a hostile file.
All three stores write atomically (tmp + fsync + rename) and archive rather
than crash on corruption.

**Chat.** `sanitize_chat` filters before truncating, caps in chars not bytes,
and no bidi or format character survives `TEXT_RANGES`. The client's `wrap`
walks `char_indices`, so multi-byte Latin-1 chat cannot panic the renderer.
`ban_store`'s matching is exact, with no substring hole.

**The client's presentation/gameplay split.** The reconciliation offset never
reaches gameplay: `body_pos()` routes both `placement_blocked` sites to
`player.pos`, and the wire and saves read `player.pos`. Only the view matrix,
aim rays, `eye_in_water` and the streaming center use `camera.pos`, all
deliberately (`NET-AUDIT.md` F2).

**Already-optimal hot paths — do not "improve" these.** The client send path
is genuinely zero-allocation steady-state (`encode_datagram_into`,
`Predictor::recycle_batch`, the reused `send_buf`/`dgram_buf`).
`FrameReader::next_frame_ref` plus in-place `write_frame` means the join-sync
decode loop borrows rather than copies. Journal region sharding, budgeted
compaction and the LRU writer cap keep boot and runtime cost tracking churn
rather than world age. `PendingEdits` allocates nothing in the common empty
case; `RemotePlayers::sample` takes caller scratch and neither sorts nor
allocates.
