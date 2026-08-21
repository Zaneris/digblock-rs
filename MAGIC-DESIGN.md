# Magic — travel, waystones & the arcane skill (design document)

Status: **DESIGN ONLY — nothing in this document is built.** It came out of the 2026-08-21 design conversation on new
skills. The owner's brief: turn return-to-spawn into a real spell with real teleport effects; unlock teleports to other
biomes at higher levels — to places the caster has genuinely never been, shared so friends who unlock them converge on
the same spot; group further spells under the skill; and make every teleport destination protected ground. Where this
document names a constant or function that does not exist yet, the name is a proposal; where it names one that does
(`spawn::lock_region`, `protect::immutable`, `Sim::teleport_client`, `ClientMsg::ReturnToSpawn`), the reference is to
shipped code and the claim about it has been checked.

Three decisions are settled here rather than open, and §4 is their record: **the spawn lock stops being an operator
toggle and is always on**, **its protected region becomes a sphere containing the old box**, and **the same protection
covers every teleport destination**.

## 1. Goals and non-negotiables

Magic is the travel-and-utility skill. Its spine is teleportation; everything else grouped under it is a world-directed
convenience in the same register. The constitution binds as everywhere else:

- **No RNG.** Destinations, costs and effects are pure functions; the one "discovery" in the design — arriving somewhere
  you have never walked — is a threshold unlock, the reveal-as-event grammar (veins fading in at a level), not a roll.
- **XP is server-computed.** One new chokepoint (`process_cast`, the `process_alchemy` twin) plus the first-arrival
  milestones, which are the DELVE-DESIGN §5 chokepoint family.
- **No combat spells.** The surface stays exactly as safe as it is today; danger belongs to DELVE-DESIGN and nothing
  here hurts anyone.
- **No spell touches per-tick movement.** Levitation, speed, water-walking — each would run through `Player::tick` and
  the adoption envelope, making every such spell its own anti-cheat surface. Teleports dodge the problem entirely: a
  teleport is a server-side position SET (`Sim::teleport_client`, the `/tp` shape), corrected by nothing because the
  server said so first.
- **Essence is the mana.** There is no mana bar and no new currency: casts are priced in essence, which makes Magic the
  standing essence *sink* the economy explicitly wants — and no cast creates matter; the one spell that writes cells
  writes temporary ones that revert through the same scheduled-write machinery that heals dungeons (§5).
- **Web is the priority platform.** Effects are `spray.rs`-mold world-space emitters — no new passes, no new render
  machinery.

## 2. The spawn spell

Return-to-spawn already exists end to end: `ClientMsg::ReturnToSpawn`, `Sim::handle_return_to_spawn` gating on
`last_return_tick` against `RETURN_SPAWN_COOLDOWN_SECS`, the cooldown persisted in `PlayerRecord` so a rejoin cannot
reset it, and `teleport_client` doing the hard part — shedding the drift EWMA, the dig hold and the stale intent,
setting `arrival_hold` and the sync scan, answering with `ServerMsg::Teleported`. The spell is a presentation-and-ladder
upgrade over that machinery, not a replacement.

- **Level 1 grants it, which means everyone keeps it.** Every skill starts at level 1, so gating the spell there is how
  an existing free feature becomes a spell without ever being taken away. Level 1 buys the dressing: a cast channel (a
  held, interruptible wind-up the client paces before sending the existing message — the server's cooldown remains the
  real gate, exactly as the doc comment on `handle_return_to_spawn` already states: honest clients gate locally), a
  departure effect, an arrival effect.
- **The ladder hangs rungs on the machinery that exists**: the cooldown steps down at thresholds (the server holds the
  caster's skills and can widen its own gate per level), the channel shortens, and the top of the line is not a better
  home teleport but the waystone network (§3).
- **The recovery spell costs no essence, ever** — the essence-tool bootstrap rule's cousin, and pinnable the same way
  (`the_recovery_spell_costs_no_essence`). Return-to-spawn is the game's recovery tool; an essence price on it would
  strand exactly the player who is lost AND broke. Its governor is the cooldown, and only the cooldown (§9).

## 3. Waystones — canonical destinations, unlocked rather than visited

The rejected shape first, so it stays rejected: *attune-by-visiting* (walk to the stone, hold, then you may teleport)
was considered and dropped. It reads well solo and sours in multiplayer — every friend must separately re-walk every
expedition before a group can convene anywhere, and the unlock stops being an event because the walk was the event. The
shipped grammar is better: a threshold fires and the world offers more of itself.

- **One canonical waystone per biome per world, derived from the seed.** `SurfaceKind` is the biome axis and is already
  documented as "the seam where the biome system attaches"; `waystones::anchor(kind, seed)` is `spawn::anchor`'s ring
  walk with a per-kind gate over the same `pristine_floor` test. Pure, stateless, and therefore **retroactive in
  existing worlds**, like ore and like dungeons.
- **The unlock IS the teleport.** Crossing the threshold makes the destination castable — no visit required, and the
  first cast is the discovery: the player materializes somewhere they have genuinely never been. First arrival at each
  waystone pays milestone XP once per account. Because destinations are canonical, two friends who cross the threshold
  hold the *same* place — "meet me at the desert waystone" is a sentence that works, and waystones become the game's
  first gathering points away from spawn. This is a direct answer to the 2026-08-16 review's finding that the world has
  no *places*, and it is the ground SOCIAL-DESIGN's claims-and-trade era will want to grow towns on.
- **The site is authored by decoration** — a small platform-and-standing-stone ruin at the anchor, the pure apron
  pattern. A purely additive decoration change is the one worldgen change that rides a `PROTOCOL_VERSION` bump with
  saves kept (`landmark_fallback_only_adds` is the shape of the proof), which is what lets waystones appear in existing
  worlds without staling anything.
- **Existing-world damage resolves once, then the answer is stored.** On the first boot that knows about waystones, the
  server validates each anchor pad against the journal (`stands_clear`) and searches sideways if players have ruined it
  — `spawn::resolve`'s mold — then stores the resolved eye in a `spawn_store`-style file (`waystones.json`) and locks
  it. Stored rather than re-derived for exactly the reason `spawn.json` stores the lock's eye: a lock's whole purpose is
  that its region cannot change. A torn ruin (players built through the pristine site before the feature landed) is
  accepted; the lock stops further damage, and the pad is what matters.
- **The ladder orders destinations by remoteness and rarity**, not curation: the common lowland kinds first, the
  mountain and snow line later, and — once DELVE-DESIGN ships — the deep sanctum ports in the tail, cast in Aether.
  Levels between destination unlocks buy the cooldown and channel rungs of §2. Everything lands inside the 1–60
  placement policy of SKILLS-DESIGN §13, with the sanctum ports as tail content by design.

## 4. Arrival regions — protection stops being optional

The decisions this document exists to record. Today `protect::immutable` takes ONE optional region — the spawn lock —
and the lock is an operator toggle (`/lock-spawn`). Teleportation makes an unprotected destination a griefing machine:
wall in the pad, roof it, drown it, dig the pit under it. So:

- **The spawn lock is always on.** Boot resolves spawn exactly as today and then locks the result automatically, storing
  the eye as `/lock-spawn` does now; `/lock-spawn` and its unlock half retire (admin commands are typed strings, not
  wire — removal is free). `/set-spawn` keeps its shipped behaviour: moving the point moves the lock with it. An
  existing world's first boot under the rule resolves against the journal first, so a dug-out anchor locks the relocated
  spawn, never the crater.
- **The lock becomes a sphere of radius `LOCK_RADIUS`, and the validated core does not move.** Growing `spawn::CLEAR`
  itself would demand a rarer pristine disc (the whole footprint must be level at one height) and change what the search
  accepts everywhere — so validation keeps its exact box and the protection changes shape around it. The sphere is
  centred at the pad's floor centre, and 6 is the derivation, quoted because the value is the argument: the current lock
  box's farthest points are its top corners at √(2.5² + 2.5² + 4²) ≈ 5.3 blocks, so radius 6 is the smallest whole-block
  sphere that contains everything the lock protects today. A full sphere rather than a dome on purpose: the lower
  hemisphere is the anti-undermining depth the shipped box never had — nobody hollows a cavern under the pad and leaves
  it a one-block shell. Membership is tested at the cell's centre, one convention both ends share so a boundary cell
  cannot be disputed. This deliberately weakens `lock_region`'s pinned property from equivalence to **containment**:
  permanence only ever needed the lock to COVER what the search reads — no permitted edit can change the search's answer
  — and the pin (`lock_region_covers_what_the_test_reads`) updates to assert exactly that direction. The cells outside
  the core are protected without being validated; they are whatever terrain they are, frozen.
- **Every teleport destination is an arrival region.** Waystone pads take the identical treatment: a `stands_clear` core
  inside the same `LOCK_RADIUS` sphere, locked at resolution and never after. One concept, one predicate —
  `protect::immutable` generalizes from `Option<&Region>` to a small set of regions, and the sphere is a second region
  shape beside the box (a centre and a squared-radius compare — a predicate swap, not new machinery). Every writer
  already funnels through the one predicate (edit validation, the fluid sim, the world-job sweeps), so the
  generalization is a signature change, not a hunt. The set is a handful of static regions; if the linear scan in that
  hot path ever shows up, it shows up in a measurement first (BENCH.md discipline), not in a guess.
- **The regions deliberately do not reach the client.** `ServerMsg::DigRefused` exists precisely for refusals the client
  cannot predict — a protected region it has no knowledge of — and waystone regions ride that as-is. Whether the client
  should eventually *render* a boundary hint is a UX question, not a correctness one (§9).
- **Claims are refused inside arrival regions** — SOCIAL-DESIGN's claim-time predicate check, the same rule it already
  states for dungeon regions, extended to one more region source.
- **Fluid is already handled**: protection refuses the fluid *write*, so water sourced outside a locked region cannot
  flow into it. Waystone pads inherit that for free.

## 5. The rest of the spellbook

Later waves, each filtered to the world-directed verb set. None of these is scheduled by this document; each is listed
so its dependency is named before somebody builds it out of order.

- **Conjured passage** — cast a temporary bridge or platform whose cells revert after a deterministic window. This is
  DELVE-DESIGN D2's scheduled-cell-write substrate used player-side, and it is what makes conjured matter honest: it
  vanishes, so "no matter creation" survives structurally. **Waits on D2.** Interaction with claims and protection:
  conjured cells are ordinary journaled writes and every writer asks `immutable` first, so protected ground refuses
  conjuration with no new rule.
- **Kindle** — cast a temporary light onto a distant cell: an emissive ramp on the same reversion clock. The RTGI is the
  whole show here; the spell is spectacle at the cost of a timer entry.
- **Prospect pulse** — the passive audio senses (`unlock::prospect_range`'s pattern) recast as an on-demand ping. Audio
  only, like the sense it echoes — the no-ESP stance holds.
- **Mending** — meaningful only once DELVE-DESIGN's health exists; a heal is the cross-skill handshake that gives Magic
  demand inside dungeons, and it consumes essence exactly where that design wants consumption. **Waits on D3.**
- **Teleport-to-friend** — the multiplayer headliner of a later wave: consent required, addressed by account id and
  never by name (the `--admin` lesson), server-side position set like every other teleport.
- **The personal waystone** — a craftable, placeable waystone inside your own claim, hand-priced as a serious essence
  sink; friends teleport to it via the claim's own co-builder allow-list. **Waits on SOCIAL-DESIGN claims.** This is
  where the exploration itch the rejected attune-model scratched comes back, priced instead of walked.

## 6. Costs — magic is the standing essence sink

- The recovery spell is free forever (§2's pin).
- Waystone casts are hand-priced per destination in the `functional_ideal` mold, with the payability pin extended to ask
  the question it always asks: **a cast must be payable at the level that unlocks it**, sourced from what that level can
  already reach.
- The common destinations price in Terra; the sanctum ports price in **Aether — a repeatable Aether sink in the tail**,
  which is SKILLS-DESIGN §13.3's own argument (the Aetherlamp/Chandelier reasoning) applied to a demand that never
  saturates, because travel is never finished.
- Cooldown and price are two governors and each spell gets ONE (§9): recovery is cooldown-governed and free; waystone
  casts are price-governed and uncooled beyond their channel.

## 7. The skill

- `Skill` is `repr(u8)` append-only; Magic is one more variant and every wire and save format already carries it. The
  skill's display name is an open question (§9) — nothing serializes a skill by name.
- **XP sources**: casts, through `process_cast`; first-arrival milestones, once per account per waystone, persisted as a
  small set in `PlayerRecord` (a format-version bump, read-compat like every other).
- **Gates**: all `unlock.rs` tables, nothing new in kind — `unlock::waystone(kind)`, the cooldown and channel rungs, and
  later the spellbook entries. The character screen's teaser machinery reads unlock tables and therefore serves Magic
  for free.

## 8. Wire and machinery inventory

- **The spawn spell needs no wire at all**: the channel is client-side pacing in front of the existing
  `ClientMsg::ReturnToSpawn`, and `ServerMsg::Teleported` already carries the answer.
- **Waystones add**: `ClientMsg::Cast { dest }` (appended), refused with the standing reject machinery when locked or
  unpaid; one join-sync message carrying the world's resolved waystones (the stored eyes are server state, so the client
  cannot derive them — the `spawn_look` precedent, sent once).
- **Remote-visible casting** rides a snapshot effect field — deliberately the SAME field the ore-draw's unbuilt remote
  half needs (SKILLS-DESIGN's standing "still design" item), so building it pays two debts at once. Until it exists,
  effects are caster-local and arrivals are simply seen.
- `teleport_client` is the landing for every spell here: residue shedding, `arrival_hold`, the sync scan and
  `clear_landing` all stand unchanged.

## 9. Hard problems, named before they bite

1. **Channels vs Delve hazards.** Once damage exists, a teleport channel is an escape rope. The rule is stated now so D3
   inherits it: taking damage interrupts a channel, through the decay mold rather than a hard reset.
2. **One governor per spell.** A cast that is both priced and cooldown-limited double-punishes and reads as neither;
   §6's split (recovery = cooldown, waystones = price) is the rule, and any new spell picks one.
3. **Solo worlds.** Always-on protection applies offline too — one rule, and the spell's arrival promise holds in every
   mode — but it means a solo player cannot build inside their own spawn region. `LOCK_RADIUS` is sized to the pad, not
   the neighbourhood, for exactly this reason. If it stings, the pressure valve is a solo settings action to relocate
   spawn (the `/set-spawn` behaviour without the admin), not a protection toggle.
4. **The skill's name.** Arcana, Leycraft, Attunement — undecided. Leystone is already in the ground and nothing yet
   explains the "ley"; a name that ties the teleport network to the ley theme gets that word for free. Display-only, so
   the decision blocks nothing.
5. **Boundary visibility.** The client never knows region bounds, so the first thing a player learns about a waystone
   pad is a refusal. Probably fine for spawn (players know spawn is special); possibly worth a rendered hint at
   waystones. UX question, deferred until the ruin art exists — the ruin itself may BE the hint.
6. **Waystone density vs biome rarity.** One waystone per kind per world is the v1 answer; a second ring of more distant
   duplicates is a later unlock wave if the map outgrows it. Derived the same way, so nothing about the wire or the
   store changes shape.

## 10. Staging

Each slice ships alone, and the first one is not even a Magic feature:

- **M-A — arrival protection** (small): always-on spawn lock, the `LOCK_RADIUS` sphere, the containment re-pin, the
  `immutable` generalization to a region set, claim refusal inside arrival regions. Pure hardening; every later slice
  stands on it.
- **M-B — the skill + the spawn spell** (small-medium): the enum variant, `process_cast`, level-1 grant, the channel,
  caster-local effects, the cooldown/channel rungs.
- **M-C — waystones** (medium): `waystones::anchor`, the decoration site, resolve-once + `waystones.json`, the two wire
  additions, first-arrival milestones, the unlock ladder.
- **M-D — the spellbook wave** (medium, gated): kindle and prospect pulse any time; conjured passage after DELVE D2;
  mending after D3.
- **M-E — the social ports** (medium, gated on SOCIAL claims): teleport-to-friend, the personal waystone.

## 11. What is NOT in this design

No combat magic, no movement-modifying spells, no RNG, no mana bar, no instanced or private destinations, no
visit-to-attune, no mounts, and no spell that permanently writes a cell — the one matter-touching spell reverts by the
same machinery that heals dungeons. Health, damage and danger stay DELVE-DESIGN's; visibility of other players'
achievement stays SOCIAL-DESIGN's. The surface world remains exactly as safe as it is today.
