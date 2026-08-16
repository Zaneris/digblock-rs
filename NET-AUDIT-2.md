# Netcode audit — performance, validation, and the master trust model

Date: 2026-08-08. Scope: `digblock-net`, `digblock-net-native`, `digblock-net-web`, `apps/server`, `apps/master`, and
the net-facing parts of `digblock-render` and the two service fillers.

Companion to `NET-AUDIT.md` (movement/positioning, 2026-08-06) and `PERF-AUDIT.md` (frame consistency). Those two are
closed; this one is the record for the round that followed. Findings that were fixed in that round live in the git
history — **this file is the standing list of what was deliberately NOT fixed, and why.**

The short version: the netcode's *mechanisms* are sound. The per-tick budgets, the interest management, the append-only
wire discipline, the adoption envelope, the predict/confirm/reject precedence — all of it holds up under adversarial
reading. What does not hold up is the **master's trust model**, which has no notion of an account. Everything in §1 is
one bug wearing seven hats.

---

## 1. Blocked on the master login flow — ALL SEVEN CLOSED (2026-08-14)

**None of these were fixable at the game server.** They all reduced to the same missing primitive: the master minted an
identity for anybody who asked for one. `POST /api/join` took a `displayName`, checked it against the name charset, and
handed back a valid token. There was no account, no ownership, no reservation, and no rate limit. Likewise
`POST /api/register` had no credential at all — the QUIC probe is a *reachability* check that everyone mistakes for an
*authentication* check.

The login flow landed, and all seven closed together, as this section insisted they had to. **`AUTH-DESIGN.md` is the
reference** for the model, the flows, the registration algorithm and its rationale, and what is deliberately still not
built. The per-finding notes below record what each one became; the prose above each is left as written, because the
description of the hole is the useful part of a closed finding.

### 1.1 Admin authority is a self-asserted display name — CRITICAL

**Closed.** Admin is `--admin <account-id>` (repeatable, or `DIGBLOCK_ADMIN_IDS`), and both `is_admin` sites compare the
account the master vouched for. A display name grants nothing, which `admin_keys_on_the_account_not_the_name` pins by
joining an impostor under the admin's exact name and asserting they get nothing.

`--admin-name` **exits** rather than being remapped onto the new flag: it was a security hole, not a spelling, and an
operator who kept using it would have been relying on the behaviour that was removed. `--admin` also accepts a display
NAME, resolved once at boot against the master and stored as the id — so renaming does not drop admin and a name claimed
later by someone else never inherits it. An unresolvable admin is fatal at boot, which is the same discipline the old
shape check had.

Name uniqueness came along for free: names are account-owned and unique at the master, so the "refuse a join whose name
is already live" suggestion is structural rather than a check.

`apps/server/src/sim/mod.rs:516-519` (and the join-time copy at `:969-972`)

```rust
let is_admin = self.admin_name.as_deref()
    .is_some_and(|a| client.name.eq_ignore_ascii_case(a));
```

`client.name` is whatever the master returned from `/api/tokens/verify` (`apps/server/src/net.rs:251-257`), and the
master will return whatever the joiner asked for. The admin name is public — it shows in the roster, in chat, in name
tags, and `just play` hardcodes `admin`. So:

```
curl -XPOST $master/api/join -d '{"serverId":"<public>","displayName":"admin"}'
```

is a complete server takeover: `/ban`, `/tp`, `/bring`, `/set-spawn`, `/set-spawn-dir`, `/lock-spawn`, `/time`.

Two things make it worse. Nothing enforces name uniqueness at join — `mod.rs:844-850` refuses a duplicate *identity*
hash, not a duplicate name — so the impersonator can be admin **while the real admin is online**. And the impersonator
can `/ban` the real admin (one live name match, different IP, so the shared-IP guard at `admin.rs:534` passes), while
the real admin cannot `/ban` back: two live matches for the name hits the ambiguity branch and refuses.

The gate itself is structurally correct — there is exactly one call site of `handle_admin_command` (`mod.rs:537`), so a
command added to the dispatch table inherits it. It is keyed on the wrong thing, and that is the whole bug.

**Fix when login lands:** key admin off the authenticated account id the master vouches for, never off the display
string. Consider also refusing a join whose display name is already live on the server.

### 1.2 Server registration is unauthenticated and evicts the incumbent — CRITICAL

**Closed**, without adding a signup step. Registration stays anonymous; the FIRST registration of a `host:port` mints a
key and the address remembers it (`ServerClaim`, the only registry state that is persisted). A later registration must
present that key while anything is live there — otherwise 409, and the source address is temp-banned for a doubling
interval, because the interesting version of this was a loop that re-fired on every victim re-registration.

A claimed address that is NOT live can be re-keyed by a registrant that passes the probes, which is the recovery path
for an operator who lost `./data`. It is safe for the same reason the probe order is what it is: passing the source
probe means being the machine at that address.

The e2e test this section flagged inverted: `a_keyless_registration_cannot_take_a_live_address` performs exactly the
attack described here and asserts the 409 and the ban, and the dirty-name half of the old test moved to
`a_dirty_server_name_is_sanitized_not_rejected`, which now presents its key like a real restart does.

`apps/master/Program.cs:64-104`, `apps/master/Registry.cs:89-92`

Registering the same `host:port` silently replaces the existing entry, taking its id and bearer key with it. The only
gate is the QUIC probe, which merely proves that *something* at that address serves a certificate matching the submitted
hash — and that hash is published to the world at `GET /api/servers` (`ServerSummary.CertHash`).

So any unauthenticated party reads a victim's `url` + `certHash`, POSTs `register` with those exact values, and passes
the probe **because the probe dials the victim**. The victim's `ServerEntry` and key are destroyed; its next
`/api/tokens/verify` gets `FindByKey → null → 401 → VerifyError::Denied`, and every join is refused until it
re-registers. The victim re-registers on its heartbeat 404, so a registration loop keeps a server permanently
unjoinable. Meanwhile the attacker controls the listed `Name` for that address and holds a valid server key.

`apps/server/tests/e2e.rs:537-541` exercises this exact primitive — re-register the same host:port under a different
name with no credential — as a *feature*. That test will need to change.

**Fix when login lands:** registration needs an operator credential, and a re-registration must present the existing
entry's key.

### 1.3 `probeHost` is client-supplied and is the only address ever dialed — CRITICAL

**Closed.** `probeHost` is gone from `RegisterRequest`. The master probes the SOURCE address first — passing it proves
the registrant is really a live QUIC endpoint at that address serving that certificate — and probes a differing
advertised address as well, with both required on the normal path. An advertised address is never published unprobed.

The dev NAT-hairpin case it existed for is `Master:Dev:ProbeHostOverride`: same effect, different authority. The master
operator sets it, not an anonymous caller.

`apps/master/Program.cs:93-97`, `apps/master/HealthChecker.cs:51`

`probe.ProbeAsync(probeHost, ...)` and the health sweep both dial `entry.ProbeHost`, while the `entry.Url` handed to
clients (`Registry.cs:36`) is built from `Host`. Register `probeHost` = your own working QUIC endpoint and `host` = any
third-party address, and the master lists — and indefinitely keeps alive — an entry pointing at an address it has never
verified, aiming every client that picks that row at the victim.

This voids precisely the guarantee `Program.cs:25-38` refuses to boot without. CLAUDE.md notes that production servers
omit `probeHost`, but nothing in the master requires or constrains that.

**Fix when login lands:** probe the advertised `Host`. If a dev-only NAT hairpin escape is still needed (the
`host.docker.internal` case), gate it behind an authenticated operator flag, not an anonymous request field.

### 1.4 The identity secret is a global bearer credential — HIGH

**Closed by deletion.** `Hello.identity` is gone (protocol v23 — the one field this protocol has ever removed rather
than appended, which is why `MIN_PROTOCOL` moved with it). A player is identified by the `AccountId` the master returns
from `/api/tokens/verify`, so identity reaches the server from the master and never from the client. There is no
replayable secret left to harvest: the join token is one-time and server-scoped, and the session token is presented only
to the master.

The design half this section deferred — "scoped per-server, or replaced entirely by the account" — took the second
option. Per-server pseudonyms are discussed and declined in AUTH-DESIGN.md §1.1.

`crates/digblock-net/src/messages.rs:130-133`, `apps/server/src/sim/mod.rs:844`

`Hello.identity` is one 16-byte secret, loaded from a single file (`apps/desktop/src/main.rs:266-297`) or one
localStorage key (`apps/web/src/lib.rs:130`), and replayed verbatim to **every** server the player joins. Server-side it
is the sole key to the `PlayerRecord` — inventory, layout, last position — with no binding to the master-verified
display name (`hash_identity`, `player_store.rs:50`).

Chained with 1.2: anyone can list a server under any name, a player joins it, and that operator now holds the player's
secret and can present it on the real server to inherit their inventory and position. The server detects only the
*concurrent*-connection case (`sim/mod.rs:845-849`), not a later replay.

The file-mode half of this (0644 → 0600) was fixed in this round. The design half is deferred: the secret should be
scoped per-server, or replaced entirely by the account the master vouches for.

### 1.5 Persisted records can be evicted by anyone — HIGH

**Closed upstream rather than here.** `MAX_IDENTITIES` is unchanged and is now honestly just a file bound: records are
keyed on accounts, an account costs a real Google or Facebook login, and `/api/join` is rate-limited per session.
Filling the store is no longer something one client can do in an afternoon.

`inventories.bin` moved to v5 for the key change alone — the record body is byte-identical. v1–v4 files are archived
rather than read forward, which is the deliberate exception to that module's own no-archive-on-upgrade rule: the old
keys are device-secret hashes with no mapping to accounts, so reading them would hand inventories to whoever happened to
collide.

`apps/server/src/player_store.rs:43,145-154`

The store keeps `MAX_IDENTITIES` records. The LRU-order bug (re-`put` never refreshing a key's position, so online
players were evicted before idle ones) was fixed in this round. The root cause was not: `identity` is client-chosen and
`/api/join` mints tokens for free at any rate, so enough join/disconnect cycles still evict every legitimate record,
oldest first.

**Fix when login lands:** key records off the authenticated account, and let the account system rate-limit account
creation.

### 1.6 Ban enforcement runs after the master round trip — MEDIUM

**Both halves addressed, one of them fully.** Bans now record the ACCOUNT as well as the IP, and either enforces — so
evasion is no longer "a new IP while the evader's inventory and position survive intact". `/unban` takes a name, an IP,
or an account id.

The ORDERING is unchanged: `verify_join` still consumes the token before the sim sees the connection, because the
account is not known until it has. What the ordering enabled — one peer holding every `MAX_CONNECTIONS` slot, each
forcing an outbound verify — is capped by `DEFAULT_MAX_CONNECTIONS_PER_IP` instead, which is the fix that actually
matches the shape of the abuse.

`apps/server/src/net.rs:251` vs `apps/server/src/sim/mod.rs:808-812`

`verify_join` — a blocking HTTP call to the master that **consumes the one-time token** — completes before
`SimEvent::Connected`/`Hello` ever reach `handle_hello`, which is where `is_banned` is finally consulted. With no per-IP
connection cap, a banned peer (or any peer) can hold every one of the server's `MAX_CONNECTIONS` slots at once and force
one outbound verify per slot per `VERIFY_TIMEOUT` window, turning the game server into an unauthenticated amplifier
against its own master.

Partially mitigated in this round by bounding the pre-Hello lifetime, which caps how long a slot can be squatted. The
ordering itself is deferred: moving the ban check earlier means the net layer needs the ban list, and the clean version
of that is to ban an *account*, not an IP.

Note also that bans are IP-only (`ban_store.rs:47`) — deliberate, but it means evasion is a new IP while the evader's
inventory and position survive intact, since those are keyed by identity. With an account system, ban the account.

### 1.7 Unauthenticated outbound probe with an error oracle — MEDIUM (SSRF)

**Closed on its own terms**, since registration stays anonymous by choice. The four `Reason` values stay — a self-hoster
needs `certMismatch` apart from `timeout` to debug a port forward — and what goes is the set of interesting targets:
`ProbeTargets` refuses anything off the public internet, checked AFTER DNS resolution, because a hostname resolving to a
private address is the whole trick. `Detail` (the raw exception text, which described the master's own network) is empty
outside Development, and the endpoint is rate-limited per source with the §1.2 temp-ban on top.

`apps/master/Program.cs:97`, `apps/master/Probe.cs:44-106`

`RegisterFailed(Reachable, Reason, Detail)` is returned to the caller with `Reason` ∈ {`dns`, `timeout`, `handshake`,
`certMismatch`} plus the raw exception message. That distinguishes "nothing there" from "something completed a QUIC/TLS
handshake" from "DNS failed", for any `host:port` the master can reach — including the Azure Container Apps internal
network it is deployed into. `Dns.GetHostAddressesAsync` is called on the supplied string with no allowlist and no
private-range filter.

**Fix when login lands:** authenticate registration (which removes the anonymous oracle), and separately collapse the
failure detail to a single opaque reason.

---

## 2. Deferred for other reasons

### 2.1 Snapshot bandwidth quantization — DONE (protocol v14)

Landed after this document was first written; kept here because the reasoning is the record of why the wire looks the
way it does.

`NetPlayerState` was one shape serving two jobs, and a large share of its wire bytes per player were provably discarded:

- `look: [f32;3]`. The only consumer was `remote.rs` — `look.x.atan2(look.z)`. **The pitch component was never read
  anywhere**, and the local player's own entry discarded `look` entirely.
- `vel: [f32;3]`, used only for extrapolation capped at `EXTRAP_CAP_TICKS`.

`pos` had to stay `f32`: `POS_EPS2` (`predict.rs`) means the echoed self position must round-trip bit-comparably or
every single snapshot takes the replay path. So the fix was a **split**, not a shrink — `SelfState` (full-precision
pos/vel, no look, no id) beside `RemoteState` (f32 pos, `i16` velocity in thousandths, `u16` yaw, held).

Two things worth remembering:

- **Yaw is interpolated in `u16` turn space, not in radians.** `b.wrapping_sub(a) as i16` IS the signed shortest arc,
  wrap included. The old code only avoided the long-way-round spin at the discontinuity because it lerped the look
  VECTOR and renormalized; lerping decoded radians instead would have introduced the bug.
- **`MAX_SNAPSHOT_PLAYERS` is sized for the WORST-CASE encoding, not the typical one.** postcard varints an `i16`/`u16`
  to 3 bytes at its extremes, not 2 — the first cut of this assumed 2, and the cap it produced was one an all-extremes
  snapshot would have overflowed. (That 3-bytes-at-the-extremes fact is a property of the encoding, not a tuning.) An
  oversize snapshot does not truncate; it fails to encode and that client gets nothing, which is the exact cliff
  interest management exists to close. `a_full_snapshot_fits_one_datagram` now pins the encoder against the arithmetic.

Measured against reconstructed v13 shapes, a realistic walking crowd (4 blocks/s, ordinary held ids):

| players | v13 snapshot | v14 snapshot |          | @20 Hz downstream |
| ------- | ------------ | ------------ | -------- | ----------------- |
| 2       | reference    | ↓ smaller    | improved | ↓                 |
| 8       | reference    | ↓↓ smaller   | improved | ↓↓                |
| 16      | reference    | ↓↓ smaller   | improved | ↓↓                |
| 24      | reference    | ↓↓ smaller   | improved | ↓↓                |

v14 is substantially smaller at every crowd size, per snapshot and per second alike, and the saving grows with the crowd
— these are encoder-deterministic sizes, not sampled timings, so there is no noise floor to clear.
`MAX_SNAPSHOT_PLAYERS` rose substantially inside the same datagram.

**The follow-up lever, also now landed — as tiering, not a smaller radius.** The snapshot selection radius is
`client.draw_dist`, and the first version of this note claimed a player is "sub-pixel" out there. That was wrong, and
worth recording because it very nearly justified the wrong fix: a player standing at the selection radius still subtends
several pixels at 1080p and 70° vertical FOV, and several times that at the near-tier distance. Small, but plainly
visible — cutting the radius would have deleted people you can see, to save bytes.

So distance tiers the RATE instead. Inside `PLAYER_NEAR_COLS` a remote rides every snapshot; beyond it, one in
`FAR_PLAYER_STRIDE`. Which snapshot is phased by player id, so far players spread across the cycle rather than all
landing in one datagram and then none.

Cost: the client interpolates a fraction of a second behind and extrapolates along velocity to the `EXTRAP_CAP_TICKS`
cap, so far players run mostly on extrapolation — worth a fraction of a block at walking speed, on a figure a handful of
pixels tall. `REMOTE_EXPIRE_TICKS` sits far outside the far tier's send interval, which a `const_assert` now pins so the
tiering can never expire the players it is merely sending slowly.

Benefit, for uniform density in the disc: most players in range sit outside the near radius, so a snapshot carries far
fewer remote entries than it used to — a bigger saving than v14's, and it compounds with v14's smaller cost per entry.

### 2.2 Join-sync pacing and the PlayerStore fsync — DONE

Both landed after this document was written; recorded because the shapes are load-bearing.

**Join sync is queued, not sent.** `handle_hello` used to encode every in-range edited column and push it in ONE tick —
thousands of frames and megabytes inside a single tick's budget on a mature world, a hitch for everyone else on the
server every time anyone joined, and past `OUT_CAP` a race the joiner simply lost (it was dropped for "overflowing the
outbound queue" through no fault of its own). Columns now go into a per-client queue that `pump_join_sync` drains a few
per tick, and only into outbound slots that live traffic is not going to need. That reserve is what makes the cliff
structurally impossible rather than merely unlikely. `JournalDone` moved ahead of the columns — the client ignores the
message, and nothing after it depends on them — so the handshake completes immediately and terrain fills in over the
next few ticks.

**The scan is region-indexed.** `JournalIndex::columns_near` walks only the regions a disc can touch. The old
`edited_columns` walk ran per client every time their center column moved — every chunk crossing — and grew for the life
of the world while every other cost around it was O(active). It returns a superset (whole regions); callers still apply
the radius test, and `columns_near_never_misses_what_a_full_scan_finds` pins that it can never return LESS than a
brute-force filter.

**Decoding is budgeted client-side too.** `NetClient::poll` used to decode the entire arrived backlog in one call. A
`JournalColumn` expands to one event PER CELL, so a few hundred columns is six figures of events materialised in a
single rAF callback on wasm — the reason the budget is in events, not frames, and why it matches the engine's own
per-frame apply budget so the backlog cannot outrun its drain.

**The PlayerStore fsync moved off the sim thread.** A flush rebuilds the whole file and fsyncs it — long enough on a
container volume to drop several ticks every interval. Encoding stays on the sim thread (it needs the live map); the
write does not. Submissions coalesce — one buffer in flight, a newer snapshot replaces an unstarted older one — so
falling behind costs staleness, never unbounded queueing, and the newest state wins, which is the only thing a
whole-file snapshot can express anyway. A failed write re-arms `dirty` through a shared flag so the next interval
retries.

### 2.3 Liveness and the untestable disconnect path — DONE

Three related gaps, fixed together because the third is what tests the other two.

**The loopback transport could not express a disconnect.** `status` was hardcoded to `Connected` and `poll` never
emitted `Disconnected`, so no in-process test could exercise ANYTHING the client does after `Welcome` when a connection
dies — coverage stopped at the pre-Welcome handshake timeout. It now has `close` (drain what is queued, then report) and
`go_silent` (deliver nothing, report nothing — a peer that hangs rather than hangs up).

That paid for itself immediately: the first test written against it caught a live regression from the join-sync work in
§2.2. Budgeting the decode had moved it AFTER the transport event loop, which silently inverted disconnect ordering —
the close was reported before the bytes that preceded it, and since the engine clears its parked queue on a disconnect,
those events were being thrown away instead of applied. `poll` now drains frames when it encounters the disconnect,
before pushing it, under one budget shared across the whole call.

**The client had no post-handshake deadline.** `check_deadlines` returned early once `ready()`, so a peer that stayed
open but stopped answering was never noticed. `RECV_TIMEOUT_SECS` is a backstop, not the primary mechanism — normally
the transport reports the death itself. It is sized off `TOD_SYNC_TICKS`: the server broadcasts time-of-day to every
client on that cadence, live or not, so even a player still waiting on their spawn column has a heartbeat. A
`const_assert` pins the relationship, and caught a first value sitting exactly ON a heartbeat boundary with no slack for
scheduling jitter.

**The native transport could not report a dead thread.** `ChannelTransport::poll` used `try_iter`, which is silent on a
dropped sender, and `status` is a cached field — so a background thread that panicked or unwound past its `disconnect`
call was indistinguishable from an idle one. It now distinguishes `TryRecvError::Disconnected` and synthesizes the
disconnect the thread never sent.

### 2.4 Client-direction frame cap — DONE

`MAX_FRAME` applied in both directions, sized for the server's journal batches. The widest thing a client legitimately
sends is a `Layout` of a few hundred bytes, but that shared cap let one push `Layout` frames at the server's whole limit
(a `Vec<u16>`, truncated only later during sanitization) straight into the sim's action queue. `MSG_BUDGET` bounded the
rate, so this was amplification rather than a heap bomb — but a cap orders of magnitude past any real message is not a
cap.

`FrameReader` now carries its own limit (`Default` keeps `MAX_FRAME` for the client reading a server; the server uses
`with_limit(MAX_CLIENT_FRAME)`). The refusal happens on the length PREFIX, before the body is buffered, which is the
property that matters. `max_client_frame_fits_every_client_message` builds each client message at its widest — a name at
`MAX_NAME_LEN`, a chat line at `MAX_CHAT_LEN` in two-byte characters, a full hotbar+grid of three-byte varint ids — so
the cap cannot be tightened into dropping honest players.

### 2.5 Contract drift — the player-count half DONE

`Contracts.cs` types `Players`/`MaxPlayers` as `int` and range-checked neither the register nor the heartbeat, while
both Rust fillers decode them as `u32`. Since the fillers deserialize the whole `ListResponse` in one shot, ONE entry
registered with `players: -1` failed the entire decode — and no client could see ANY server on that master.

Fixed on both sides, because either alone leaves a gap: the master clamps (`ServerRegistry.SanitizeCount`, applied at
register AND heartbeat) so the bad value never leaves, and the fillers clamp on the way in so a stale or hostile master
cannot do it either. A count is display text beside a name; it was never worth the whole listing.

Pinned by an e2e test that registers by hand with `players: -1` — a well-behaved client cannot send it, since
`RegisterBody` is `u32` — and asserts the listing is still readable. Verified to fail against the unclamped master.

**This section's leftovers — resolved with §1, as predicted.**

- `ProtocolVersion`/`SimVersion` now ride `ServerSummary`, pinned by `the_listing_carries_protocol_and_sim_versions`. A
  client can tell an incompatible server apart before spending a QUIC connect on it.
- `JoinResponse.ExpiresIn` is still read by neither filler. Left alone: the token's life is far shorter than any
  client-side use for the number, and both fillers redeem it immediately.
- The display-name item **dissolved** rather than being fixed. Names are account-owned and unique at the master, chosen
  through `POST /api/account/name`, and the joiner cannot pick one per request — so a name is no longer an authorization
  input anywhere, which was the whole reason its validation mattered. `sanitize_name` stays as display hygiene.

### 2.6 The small ones — DONE

- **Command-vs-chat was decided on unsanitized text and executed on sanitized text.** They disagree for any character
  the sanitizer strips that `trim_start` does not count as whitespace — `U+200B` is exactly that: not `White_Space`, not
  inside `TEXT_RANGES`. So `"\u{200B}/ban griefer"` read as ordinary chat, sanitized to `/ban griefer`, and was
  BROADCAST to every player: a non-admin publishing a command, or an admin leaking one they meant to run. The handler
  now sanitizes FIRST and decides from the result, so both halves read the same string by construction rather than by
  agreement.

- **`--admin-name` was never validated.** Admin is granted by matching against a display name, which the master
  constrains to `validate_name` (3-10 chars of `a-z A-Z 0-9 - _`), so `--admin-name "big boss"` could never match anyone
  — the server booted with admin silently disabled while logging a reassuring `admin: big boss`. The launcher now exits
  on it, and `set_admin_name` refuses to store one, so neither entry point can leave a server in that state. (This
  validates the SHAPE only; names remain self-asserted, so admin stays impersonable until §1.1.)

- **`noop_catch` was dropped while write promises were still pending.** `close()` rejects every in-flight write, and
  those rejections are microtasks that fire after the wasm call returns — by which point a client-owned closure has been
  dropped with the struct, so wasm-bindgen threw "closure invoked after being dropped" from inside a catch handler on
  every disconnect. The handler is now module-lifetime (`thread_local!`), owned by nothing that can go away first.

---

## 4. Still open

**Nothing from §1 or §2.5.** The login flow landed on 2026-08-14 and closed all of it; see `AUTH-DESIGN.md`, whose §7 is
the standing list of what that work deliberately did NOT build.

One thing the accounts work surfaced, **since fixed** and recorded here because the shape of it is worth keeping:
**nothing sent `ClientMsg::Bye`**. The variant existed and the server handled it by dropping the client at once, but no
client emitted one, so an abruptly-closed connection was only reaped on QUIC's idle timeout. Harmless while the
duplicate check keyed on a secret a player might not have; not harmless once one account holds one live connection,
because then the session being held belongs to the player trying to rejoin.

The fix is `NetClient::disconnect`, called from every deliberate exit. The non-obvious half is that sending the message
is not enough: `write_all` buffers into the QUIC stream, and dropping the transport closes the connection before it
drains. So the native transport finishes the send stream on the way out and `close()` blocks briefly for it — a detached
network thread otherwise loses the race with process exit, which is exactly the case that matters.
`a_goodbye_frees_the_account_at_once` pins it over real QUIC with no sleep and no retry; an earlier draft of that test
needed a twelve-attempt retry loop and still timed out, which is what the missing flush looked like from the outside.

§4.1 has since been fixed and is kept here rather than moved, because the two diagnoses it had to walk back are the
useful part. What it leaves open is server-side **aim** lag, called out at the end of it.

### 4.1 Input delivery under jitter — FIXED (`INPUT_HOLD_MAX`)

Input delivery used to fall sharply at the two worst links — a visible shortfall at 300 ms/5% and a severe one at 500
ms/10%. Jitter caused it, not loss: heavy jitter reorders datagrams, and `handle_inputs` skips anything at or below
`last_input_tick`, a high-water mark, so a tick arriving after a newer one had been consumed was discarded rather than
applied out of order. Redundancy could not help — every copy of a tick is equally old.

An earlier draft blamed `JITTER_BUFFER_TICKS` for being "too small to absorb the reordering". **That was the wrong
diagnosis**, and worth keeping on the record. The constant is only the threshold above which the drain takes two inputs
in a tick; the drain always took at least one when the buffer was non-empty and had no way to take zero, so depth was a
one-way ratchet — the `depth` column sat between one and two ticks even on a clean LAN, and that residue was entirely
`INPUT_SEND_STRIDE` burstiness rather than absorption. Raising the constant would have changed nothing.

**The fix is `INPUT_HOLD_MAX` (`movement.rs::step_physics`):** the drain holds a *hole* in the tick sequence open for up
to `INPUT_HOLD_MAX` ticks, so the late input is still admitted. Delivery recovers most of the shortfall on both bad
links. The lan and wifi rows are unchanged to the digit — no reordering, no holes, no holds — which is what makes this
free for almost every player. The measurements, the cap sweep it was chosen from, and the honest debits are in BENCH.md.

Two of the three caveats in the previous draft held up; one did not.

- **The adoption envelope already absorbed most of the positional damage.** Held. It is why this was polish and not an
  emergency.
- **What the envelope does not repair is `client.look`.** Still true, still unfixed, and the new `dig ok/att` column
  shows it: at 500 ms a hold can exceed `DIG_OFF_TARGET_RESET`'s consecutive-off-target limit and be abandoned outright,
  while `dig short` stays zero — so it is aim lag, not `dig_grace`, that fails there. Aim has no adoption path. **This
  is the remaining open item on this path**, and the handful of attempts a bench row currently gets is too small a
  sample to size it.
- **"A deeper buffer cannot land without making `dig_grace` and `EDIT_POS_TOLERANCE` depth-aware."** **Wrong**, and the
  reason matters. The measured `orig` residual *falls* under a bounded hold — at 300 ms and at 500 ms alike, and by a
  margin well past the bench's run-to-run repeatability (both tables are bit-identical run to run) — because the
  alternative to holding is coasting on stale intent and a coast diverges faster than a delay. Making those constants
  elastic would also have been the one version of this change with real cheat surface: depth is client-controlled
  through tick numbering, `verify_digs` casts `REACH + EDIT_POS_TOLERANCE`, and scaling the tolerance with the deepest
  hold the sweep reached would have bought better than a block of extra reach for anyone willing to look like a bad
  connection. **Their fixedness is a standing constraint, not an oversight** — it is recorded as a rule on the constant.

The server-side counters that made this falsifiable (`Sim::input_stats` and `Sim::edit_stats`) remain purely diagnostic
— nothing reads them back into a decision, including the hold, which keys only on whether the next tick is present.
