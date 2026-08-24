# Compatibility — joining a server that has not updated (design document)

Status: **C1 (§7) is BUILT; everything else here is design only.** It came out of the 2026-08-23 question of what
happens once dedicated servers are run by people who did not ship the release. They are still refused — C1 changed only
how honestly — and every way of changing that carries a cost worth choosing deliberately rather than discovering. Where
this document names a constant or a function that does not exist yet, the name is a proposal; where it names one that
does (`MIN_PROTOCOL`, `dig_grace`, `landmark_fallback_only_adds`), the reference is to shipped code and the claim about
it has been checked.

The staging (§8) is ordered so each slice ships alone, and the one that shipped buys no compatibility at all — it makes
the failure legible, which everything after it depends on.

## 1. What a join is gated on today

Two constants, both exact:

- **`PROTOCOL_VERSION`, the wire shape.** `MIN_PROTOCOL` currently sits AT it, so the accepted window holds exactly one
  version. Checked twice on each side — the preamble gate in `apps/server/src/net.rs`, then again in
  `Sim::handle_hello`, because the sim is also driven directly.
- **`digblock_core::SIM_VERSION`, the behaviour both ends run independently.** Equality, refused from either side: the
  server on `Hello`, the client on `ServerMsg::Welcome`.

Three properties of the shipped code decide what is cheap here and what is not, and all three lean toward compatibility:

- **The preamble is frozen forever.** Magic, then the version, then the reserved bytes, ahead of any postcard frame. Any
  two builds — past or future — can always exchange versions and refuse each other cleanly. The negotiation machinery
  exists; only the accepted range is empty.
- **An undecodable frame is skipped, not fatal.** `net.rs`'s drain logs it and continues, and `NetClient::drain_frames`
  does the same. Only a framing corruption ends a session. `codec::decode` is `postcard::from_bytes`, which rejects
  trailing bytes, so a shape skew is a clean error on both sides rather than a silent misparse.
- **Unknown world data already degrades.** `BlockType::from_id` reads an out-of-range id as Air, and `try_from_id` is
  its strict twin for validation paths. A newer block reaching an older client is a hole in the world, not a crash.

And there is already one precedent for the whole idea: `AlchemyOp::Convert` is a retired verb **kept decodable and
refused**, answered with the standing rebase so an out-of-date client at least resyncs. That is what an append-only
wire's graceful degradation looks like, and there is exactly one of it.

## 2. Why the floor keeps rising

MAGIC-DESIGN §8 names the two reasons, and the history bears them out: in under a month the wire reached v36 and the
behaviour gate moved nearly as often. That is pre-release velocity and not a rate a released game would sustain — but
the SHAPE of it is what matters, and it is two shapes:

- **A field appended to an existing variant.** The one genuinely breaking postcard change: a decoder expecting the byte
  runs off the end of an older peer's frame, and an older decoder finds trailing bytes it cannot account for. v27's
  `DigStart.slot` and v32's `RemoteState.fx` are each a floor raise for exactly this and nothing else.
- **A tag whose meaning moved.** The dangerous one. `WardWinding` took the retired `Warded`'s discriminant, so an older
  peer would decode it *successfully* and read the seal. Refusing the handshake is the honest answer, because nothing
  downstream can catch it.

There is a third thing the floor gets raised for, and it is not a decode problem at all: a newer client offering a
feature an older server cannot decode. The session survives — the frame is logged and dropped — so what the player gets
is a spellbook row that silently does nothing. That is a worse experience than a refusal, which is why the floor moved
with the Causeway. §4.4 is the cheaper answer to it.

## 3. The asymmetry that sets the priority

Clients and servers do not update at the same speed, and they do not lag in the same direction.

- **The web client is always current.** The bundle is content-stamped and only the always-revalidated `index.html` knows
  the stamps, and web is the priority platform.
- **Desktop and iOS lag by a download and by an App Store review.**
- **A dedicated server updates when its operator feels like it**, which is this document's whole premise.

So the case to design for is **a current client against a stale server**, and the rarer one is a stale client against a
current server. That ordering is load-bearing twice over: it is what makes §6 — which helps only the web client — a
serious option rather than a curiosity, and it is why capability negotiation (§4.4) is asymmetric in practice, since the
client is the side that must dim its own features.

## 4. The wire axis

### 4.1 Additive-only discipline, enforced rather than remembered

Never append a field to an existing variant; append a new variant instead. The floor then rises only for §2's second
reason, and can sit years behind.

Enforcement is the point, because this is a rule that gets forgotten under a deadline. A golden of
variant-to-encoded-shape per supported version fails the suite when an existing variant's shape moves, and passes when
the floor deliberately moves with it. That is the shape the writing rules already ask for: wire goldens are the named
exemption to deriving an expectation, and one that moves is reporting a decision rather than echoing a definition.

Cost: variant sprawl, and a `send_for(peer_version)` fork per evolved message — the client already learns the peer's
version from the preamble before it sends `Hello`, so the choice is always available. Buys: most of the historical
raises stop happening.

### 4.2 One forward-compatibility envelope, once

Append `ext: Vec<u8>` to the handful of variants that actually evolve — `Hello`, `Welcome`, `EditRequest`, `DigStart`,
`RemoteState`, `InputTick` — and put future fields inside it as a nested postcard struct. Postcard length-prefixes a
`Vec<u8>`, so an older decoder reads the envelope and skips what it holds. One final floor raise now; field additions
never raise it again.

The cost is bytes, and where they land decides whether it is worth paying: `InputTick` and `RemoteState` ride every tick
per player, so the envelope belongs on the reliable control stream while §4.1's discipline stays on the datagrams. A
measured pass under BENCH.md discipline settles that before it ships rather than after.

The second cost is subtler and worth naming: an envelope is a place where one field can exist twice, once in the struct
and once inside. The rule that keeps it honest is that a field moves INTO the envelope only when the floor has risen
past every peer that read it outside.

### 4.3 Version-parameterized codecs

Keep the structs ergonomic and write `encode_as(version, msg)` / `decode_as(version, bytes)` with per-version adapters,
each pinned by a byte golden. Total freedom to change shape, at the highest maintenance cost on this list — and only
sane with a hard window and a schedule for deleting the oldest adapter. If the window is that small, §4.1 buys most of
the same range for a fraction of the work. Recorded as the fallback for a change §4.1 and §4.2 genuinely cannot express,
not as a plan.

### 4.4 Capability negotiation, so a wide window is worth having

A version range says a session can be established. It says nothing about whether the game makes sense inside it, and
§2's third failure — the row that silently does nothing — is what a range buys more of if nothing else changes.

The answer is a feature set exchanged in `Welcome` (or carried on the master's `ServerSummary`, which already carries
versions), and a client that dims what the peer lacks: the spellbook greys the row, the tooltip says why, and the cast
never leaves. `ServerMsg::CastRefused` already carries a `CastRefusal` rather than a sentence and `voice` already
chooses the words, so a refusal that names an absent capability is one more variant on a road that is already paved.

The cost is real and is mostly UI: every optional feature needs a state in which it visibly is not there. That is the
honest price of supporting a range, and it is worth paying once the range exists rather than speculatively — but
`Welcome` should be shaped to carry the set at the next deliberate floor raise, because the floor is going to rise
anyway.

### 4.5 The rule that holds whatever else is chosen

**A retired discriminant is reserved forever, never reused.** Every other option here degrades to a clean refusal or a
dropped frame; tag reuse degrades to an older peer confidently reading the wrong message. A test that walks the message
enums against a committed roster of retired tags is cheap, and it is the only one on this list that catches a silent
wrong answer instead of a loud failure.

## 5. The behaviour axis

`SIM_VERSION` is one constant over four things whose tolerance for skew differs by orders of magnitude:

1. **Worldgen.** Zero tolerance. Terrain is never sent — join sync is seed plus edits — so a difference is silently
   different worlds.
2. **`Player::tick`.** Low tolerance. The adoption envelope corrects, but a real change means claims rejected in bulk
   and a permanent correction offset.
3. **The tables both ends time a dig against.** Some tolerance: `dig_grace` absorbs jitter, and every seam errs in the
   server's favour by construction.
4. **Unlock thresholds and the economy.** The server is authoritative already — XP at its three chokepoints, essence
   through the standing rebase. A skew here is a mispredicted button, not a divergent world.

Read the bump history against that split and the lever is obvious. The terrain-region and bedrock changes are (1); the
capsule and the stair motion are (2). Almost everything since — the dig curve, the strength wall, each catalog wave, the
pick, bench extraction's removal, the stubborn draw — is (3) and (4). **Most joins would be refused for reasons that do
not require refusing a join.**

### 5.1 Split the constant

`WORLDGEN_VERSION` and `PHYSICS_VERSION` gate on equality; (3) and (4) leave the gate entirely.

The obvious objection is that three constants are three chances to bump the wrong one. The answer is that neither of the
two that matter is bumped by judgment: `worldgen_golden_hash_pins_sim_version` and the three
`golden_*_trajectory_pins_sim_version` tests already fail on drift and already name the constant in their message.
Splitting it is mostly re-pointing goldens that exist and deleting a gate.

What (3) and (4) then cost is a client that mispredicts against an old server: a dig bar that finishes a little early or
late, a button greyed one level off. §5.2 removes the first. The second is the price of the window — and it is a price
the alternative, refusing the join, charges in full.

### 5.2 The tables as join-sync data

The stronger form of the same move: the server sends the rows both ends time a dig against — per-block hardness and
`xp_ticks`, the shared taper, the unlock thresholds — and the client's timing becomes data rather than code. Sized by
`BlockType::COUNT` at a few bytes a row, it is smaller than one journal column and rides the join burst that already
carries `Skills` and `Layout`.

It is not free, and the cost cuts against a house rule: a number would then live both in the code that defines it and in
a copy the client was handed, and every `hardness_ticks()` call site goes from a const lookup to an instance one.
Offline play keeps the local tables regardless, which is also the argument for doing it — both copies exist either way,
so the received one simply becomes what multiplayer reads while the local one stays what offline reads.

Sequence it after §5.1. The split is what makes the window exist; this is what makes dig timing CORRECT inside it rather
than merely tolerated.

### 5.3 Freeze worldgen after 1.0

The zero-tolerance axis has to stop moving or no window is possible at all. After 1.0, worldgen changes are **provably
additive only** — and the proof already has a shape: `landmark_fallback_only_adds` pins a change that can only ADD,
which is why a decoration change can ride a protocol bump with saves kept.

This is a product decision wearing an engineering hat, and it should be made rather than discovered: it means never
retuning terrain on a live world again. The alternative is not "retune freely" — it is "retune, and strand every
existing world and every server still on the old build", which is what happens today.

### 5.4 Rejected: N generators in the client

Ship `worldgen::v23::build_column` beside `v24`, selected by what the server advertises. Recorded so it stays rejected:
worldgen is not one function. Terrain, caves, rivers, lakes, classification, decoration and the node layer would each be
versioned alongside it, with goldens for each, forever — and the mesher, the reveal pass and the node anti-dupe all read
the results. The cost is out of proportion to everything else here, and §5.3 buys the same outcome by not needing it.

## 6. Sidestepping the whole problem, on web

The web deploy already produces exactly what a version-matched client needs, for reasons that had nothing to do with
compatibility: `just build-web` content-stamps each glue/wasm pair, every pair is immutable by name, and only
`index.html` knows the stamps. The master already stores each server's protocol and sim version. Sessions live in
`localStorage` and survive a reload; the sign-in residue that must not is per-tab in `sessionStorage` by design.

So: the master maps a version pair to a bundle, and a web client that picks a known-good older server reloads into the
bundle that was tested against it, carrying the join intent in the query. The page already refuses an unknown `?key`, so
that intent is one more `PAGE_PARAMS` entry rather than a new mechanism.

What this buys is unusual — not a negotiated window but **the exact pair that was tested together**, at arbitrary
version distance, with no protocol work at all. On the priority platform it dissolves §4 and §5 entirely.

What it costs:

- A mapping the master owns and a deploy step that maintains it, plus a retention policy for old bundles. The artifacts
  are immutable and cheap; the policy is the real decision.
- One page reload mid-menu, which on a cold mobile load is not instant. The loading overlay's staged watchdog already
  covers the honest version of that.
- The player runs an old client on that server, so client-side fixes do not reach them, and "which build am I on"
  becomes a question the game has to answer on screen.
- The master becomes load-bearing for something it is not today. Join already fails closed against a master outage; this
  would make the SERVER LIST version-aware, which is a wider dependency than the one that exists.
- Desktop gets nothing without a launcher that downloads matching builds, and iOS cannot do it at all. That asymmetry is
  acceptable only because §3 says which platform matters.

## 7. Telling the truth, which ships first regardless — BUILT

The master had accepted `ProtocolVersion` and `SimVersion` at registration and `ServerSummary` had carried both to the
client since before this document, and **neither client parsed them**: an incompatible server was indistinguishable in
the list, and the player found out only after a master round trip, a QUIC connect and a preamble exchange.

What shipped is `protocol::compat`, ONE function answering what both handshake gates would answer, asked before anything
is dialed. That single-sourcing is the point rather than a tidiness: the preamble check, the sim's own handshake and the
menu's verdict now call `protocol_supported` / `sim_supported` instead of each open-coding the comparison, so a listed
row cannot promise a join the handshake then refuses. Ahead on either gate reads `PeerNewer`, because the side that must
update is the side lacking something.

Around it: the two fields on `ServerEntry`, clamped on the way in — a version that cannot be read must land OUTSIDE the
window, which is the opposite of a player count's failure and is why `lenient_version` is its own function; a row the
gate refuses keeps its place in the list, takes the reason where its ping would go, and stops being an offer, so
selection skips it and no token is spent; and neither launcher probes a server it could not join. `WorldRow::compatible`
was the pattern, and the menu owns the label, since a menu label is layout as much as it is text.

This buys no compatibility whatsoever. It went first because every option above needs the client to know a server's
versions before it dials, and because the failure it replaced is the one a self-hoster's players would report as a bug
in the game.

## 8. Staging

Each stage stands alone and none of them presumes the next.

- **C1 — the honest list (§7). SHIPPED.**
- **C2 — split the behaviour gate (§5.1) and freeze worldgen (§5.3).** The decision with the most leverage here, and
  mostly a re-pointing of goldens that already exist.
- **C3 — additive-only discipline with its shape golden (§4.1), plus the reserved-discriminant test (§4.5).** Mechanical
  and cheap, and the second is the only guard on this list against a silent wrong answer.
- **C4 — the feature set in `Welcome` (§4.4).** Shaped in at the next deliberate floor raise, lit up per feature as
  features need to degrade.
- **C5 — tables as join-sync data (§5.2)**, when a mispredicted dig bar against an old server is a complaint rather than
  a theory.
- **C6 — version-matched web delivery (§6)**, if a genuinely wide window is wanted without paying for it in protocol
  complexity.

§4.2's envelope is deliberately unplaced. It is the right answer if field-append keeps being what raises the floor after
C3 lands, and the wrong one if C3 alone holds — a question the next year of bumps answers, not this document.

## 9. Hard problems, named before they bite

1. **A window is a test matrix.** Two supported versions means two server builds in the e2e suite and a peer-version
   axis through the loopback tests. The expensive part is not the code that speaks the old version, it is the CI that
   proves it still does. A staging that widens the window without widening the suite is widening the surface for free,
   and nothing is free.
2. **Behaviour skew is invisible exactly where it is worst.** A protocol mismatch fails loudly; a table skew shows up as
   a dig bar that lies. §5.2 fixes the timing half. The unlock half — a button greyed one level off — has no mechanical
   fix, so the client must be able to say WHY it disagrees rather than merely disagreeing, which is `voice` work and
   belongs with C5.
3. **The journal and the save format ride along.** `digblock-persist`'s `VERSION` is shared by server volumes and client
   saves, and a worldgen retune stales both together. §5.3's freeze is what keeps that from becoming a second
   compatibility axis; without it an old server's world is unreadable by anything, which no amount of wire work reaches.
4. **An old server is a security surface.** Supporting a version range means supporting the anti-cheat that shipped with
   it. `dig_grace`, `EDIT_POS_TOLERANCE` and the adoption envelope's bias detection are all server-side, so an old
   server is exactly as strict as it was — which is that operator's business. What must not happen is a NEWER server
   relaxing a check to accommodate an older client. The rule: **a window may widen what a server DECODES, never what it
   ACCEPTS.**
5. **Deprecation needs a channel.** Whatever window is chosen, an operator needs to know they are near its edge before
   their players do. The heartbeat response is the obvious carrier and costs one field. The policy behind it — warn,
   delist, or refuse to mint tokens — is a separate decision and belongs to whoever runs the master.

## 10. What is NOT in this design

- **Any leniency about worldgen or physics.** Both stay exact-match under §5.1. A "close enough" world is the one
  failure mode with no observable symptom until two players describe different terrain to each other.
- **Sending terrain.** Join sync is seed plus edits, and every streaming, worker and budget decision downstream assumes
  it.
- **A self-describing protocol.** Postcard's compactness is load-bearing on the datagram path; the answer to evolution
  is discipline and, at most, §4.2's bounded envelope.
- **Server-side support for a client NEWER than itself, beyond decoding.** An old server never learns a new verb.
  Everything a new client wants from one must degrade to something it already understands, or be dimmed by §4.4.
- **A launcher or auto-updater for desktop.** Named in §6 as the native counterpart to version-matched delivery, and
  scoped out: it is an installer project, not a compatibility one.
- **Any window at all before 1.0.** None of this is worth building while the floor moves weekly and there are no
  third-party servers to strand. §7 is the exception, and it is worth having today for a failure that exists today.
