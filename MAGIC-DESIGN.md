# Arcana — travel, waystones & the arcane skill

Status: **BUILT** (branch `feat/arcana`, stages 1–8). This document came out of the 2026-08-21 design conversation on
new skills and is now the record of what shipped and why, plus §9's open questions and §10's unbuilt tail. The owner's
brief: turn return-to-spawn into a real spell with real teleport effects; unlock teleports to other biomes at higher
levels — to places the caster has genuinely never been, shared so friends who unlock them converge on the same spot;
group further spells under the skill; and make every teleport destination protected ground. Where this document names a
constant or function, the reference is to shipped code.

Decisions settled during the build, so they stay settled: the skill is named **Arcana** (`Skill::Arcana`); casting lives
in a **spellbook screen** (J; Return Home is cast from the book like every spell, and the pause menu's old "Return to
Spawn" row now opens it); **the spawn lock stopped being an operator toggle and is always on**, its region a **sphere**
containing the old box, and the same protection covers every teleport destination; waystone **structures are STAMPED by
the server as ordinary journaled edits, never worldgen** (§3); and the teleport visuals are their own
helix-and-column-of-light shaders, deliberately **not the confetti look**.

## 1. Goals and non-negotiables

Arcana is the travel-and-utility skill: Mining breaks blocks, Alchemy transmutes them, **Arcana moves things that aren't
blocks — the player, and light.** The constitution binds as everywhere else:

- **No RNG.** Destinations, costs and effects are pure functions; the one "discovery" in the design — arriving somewhere
  you have never walked — is a threshold unlock, the reveal-as-event grammar, not a roll.
- **XP is server-computed.** One new chokepoint, `process_cast`, the `process_alchemy` twin; offline arms award through
  the same core functions in the offline branches only.
- **No combat spells.** The surface stays exactly as safe as it was; danger belongs to DELVE-DESIGN.
- **No spell touches per-tick movement.** Levitation, speed, water-walking — each would run through `Player::tick` and
  the adoption envelope, making every such spell its own anti-cheat surface. Teleports dodge the problem entirely: a
  teleport is a server-side position SET (`Sim::teleport_client`), corrected by nothing because the server said so
  first. **A spell may READ the tick and never rewrite it** — the Causeway is the line's one near miss and stays the
  right side of it: the caster's physics are untouched, only the world under them changes, through the ordinary edit
  path. What is new is that its TRIGGER reads per-tick movement state (`Player::grounded`, and nothing else), which is
  why the distinction is written here rather than left to be re-derived.
- **Essence is the mana.** No mana bar, no new currency: casts are priced in essence, which makes Arcana the standing
  essence *sink* the economy explicitly wants. No cast creates matter — the light spells write nothing to the world at
  all (§5), and the one that writes real blocks (the Mason's Arc) spends the player's own inventory.
- **Web is the priority platform.** Effects are instanced world-space emitters in the additive water-pass slot — no new
  passes, no new render machinery, WGSL that passes real Chrome's front end.

## 2. The spawn spell — Return Home

Return-to-spawn kept its machinery (`ClientMsg::ReturnToSpawn`, `Sim::handle_return_to_spawn`, `teleport_client`) and
became a spell in front of it:

- **Level 1 grants it, which means everyone keeps it.** The spell is a held, interruptible channel
  (`unlock::channel_ticks`, shortening at `CHANNEL_QUICK_AT`) the client paces before sending the existing message — the
  server's cooldown remains the real gate. Movement, opening a screen or starting a dig RESETS the channel
  (`Msg::CastInterrupted`); the decay mold is reserved for D3 damage.
- **ONE cooldown covers every teleport** (`unlock::teleport_cooldown_secs`; the level-1 answer IS the old flat constant,
  aliased rather than copied), read per-caster by the server and stepping down at the two swift rungs. Return Home and
  every waystone port rest on the same clock, because per-spell timers let a player ring the roster and land home with
  nothing left to wait for. Client-side one predicate decides who rides it — `CastTarget::teleports`, asked rather than
  restated at each gate, so a future travel spell cannot quietly arrive outside the cooldown.
- **The recovery spell costs no essence, ever** (`the_recovery_spell_costs_no_essence`) — the essence-tool bootstrap
  rule's cousin: an essence price would strand exactly the player who is lost AND broke. Its governor is the cooldown,
  and only the cooldown (§6).
- **A homeward jump pays distance-scaled XP** (`skills::home_xp`: linear in blocks travelled, capped). This is the
  ladder's bootstrap — a fresh player who has never touched Alchemy still climbs — and the anti-idle rule is arithmetic,
  not policy: recasting at spawn moves zero blocks and pays zero.

## 3. Waystones — canonical destinations, unlocked rather than visited

The rejected shape first, so it stays rejected: *attune-by-visiting* (walk to the stone, hold, then you may teleport)
was considered and dropped. It reads well solo and sours in multiplayer — every friend must separately re-walk every
expedition before a group can convene anywhere. The shipped grammar is better: a threshold fires and the world offers
more of itself.

- **One canonical waystone per biome kind per world, derived from the seed.** `waystones::Roster` is `spawn::anchor`'s
  ring walk generalized (shared accept-closure walk, per-kind surface gate). The roster is the seven kinds
  `unlock::waystone` answers for; GravelShore and Scree are excluded — a narrow shore band and a slope patch are not
  destinations.
- **No two arrival pads crowd each other.** Every site — spawn included — stands at least `waystones::MIN_SEPARATION`
  from every other, which is why the roster resolves as a SET rather than a function per kind: the rule is mutual, so a
  pad can only be judged against the pads already placed and a per-kind answer would depend on who asked first. Rungs
  are placed lowest-first (order derived from `unlock::waystone`, never written down), so the first port a player
  unlocks gets the nearest site the map can offer and later rungs are pushed outward; a rung resolves lazily and its
  prefix is exact, so an interactive cast pays one near ring walk rather than the whole ladder's. The earlier rule was a
  minimum distance from the world ORIGIN alone, which let the roster cluster: several kinds took the first ring past the
  floor and landed within sight of each other and of spawn.
- **The unlock IS the teleport.** Crossing the threshold makes the destination castable — no visit required, and the
  first cast is the discovery. A first arrival pays exactly what every later one pays: the fare's worth, and nothing
  else. The rejected shape, so it stays rejected: a once-per-account first-arrival MILESTONE. A flat table of them could
  not hold a constant stride against an exponential curve — the first port paid three levels and the last paid less than
  one — and the per-account memory it needed (`visited_waystones`, a field in the player record) existed for nothing
  else. A discovery is its own reward; the XP is the drain. Because destinations are canonical, two friends who cross
  the threshold hold the *same* place — "meet me at the mountain waystone" is a sentence that works, and waystones are
  the game's first gathering points away from spawn.
- **The site is STAMPED, not generated.** The earlier draft authored the ruin by decoration; that was superseded because
  stamping is strictly safer for existing worlds: `waystones::stamp` is a pure builder returning cell writes — a
  platform replacing the ground under the pad, footed shafts, lamp caps — and the server applies them **as ordinary
  journaled edits** at boot, exactly once per site (idempotent by journal-equality skip). No worldgen changes, no moved
  goldens, no additive-decoration proof, and retroactive in existing worlds by construction, since join sync is seed +
  edits. **Spawn gets the same stamp** — the accepted one-time spawn move on an old world — and **`/set-spawn` re-stamps
  at the new site**, which is the admin-moves-spawn flow. The old site's blocks stay as ordinary now-unprotected edits.
- **Existing-world damage resolves once, then the answer is stored.** First boot validates each anchor pad against the
  journal (`stands_clear`), resolves sideways if ruined (`spawn::resolve`'s mold), stamps, stores the eye in
  `waystones.json`, and locks it. Stored rather than re-derived because a lock's whole purpose is that its region cannot
  change. Damage resolution is also the one thing that can nudge a stored pad inside another's exclusion — by a few
  blocks, and the alternative is a port standing in a crater.
- **A stored roster moves for exactly one reason: the placement rule itself changing.** `waystones::PLACEMENT_RULE` is
  stamped into `waystones.json`; a file written under an older rule is archived beside itself and re-placed once, and
  the structures the old sites stamped stay standing as ordinary now-unprotected edits — the same answer `/set-spawn`
  gives when it moves spawn out from under a pad.
- **Every site also wears a standing marker** — the slow rising spiral of `waystone.wgsl`, distance-culled client-side
  over every resolved site. The spiral doubles as the protection-boundary hint (§9's old open question, answered).

## 4. Arrival regions — protection stopped being optional

- **The spawn lock is always on.** Boot resolves spawn as before and locks the result automatically; `/lock-spawn`
  retired (its `AdminReply` variants stay decodable — the wire is append-only), `/set-spawn` keeps moving the point, the
  lock, and now the stamp with it.
- **The lock is a sphere of radius `spawn::LOCK_RADIUS`, and the validated core did not move.** Growing `spawn::CLEAR`
  itself would demand a rarer pristine disc and change what the search accepts everywhere — so validation keeps its
  exact box and the protection changed shape around it. The radius derivation, quoted because the value is the argument:
  the old lock box's farthest points are its top corners at √(2.5² + 2.5² + 4²) ≈ 5.3 blocks, so 6 is the smallest
  whole-block sphere containing everything the box protected. A full sphere rather than a dome on purpose: the lower
  hemisphere is the anti-undermining depth the box never had. Membership is an integer squared-distance compare on
  whole-cell coords, one convention both ends share. This deliberately weakened `lock_region`'s pinned property from
  equivalence to **containment** — permanence only ever needed the lock to COVER what the search reads — and the pin
  asserts exactly that direction.
- **Every teleport destination is an arrival region**: the identical sphere, locked at resolution and never after.
  `protect::immutable` takes a slice of regions; `Region` is a two-variant enum (box, sphere). Every writer already
  funnels through the one predicate (edit validation, the fluid sim, the world-job sweeps), so the generalization was a
  signature change, not a hunt.
- **The regions reach the client only offline.** Online, `DigRefused`/`Protected` covers refusals the client cannot
  predict, as everywhere; offline, the engine derives the same regions and enforces locally so the two modes agree.
- **Fluid was already handled**: protection refuses the fluid *write*, so water sourced outside a locked region cannot
  flow in. Waystone pads inherit that for free.

## 5. The spellbook — what casts today

The book (J) is one screen in the alchemy-page mold: a pure `SpellView::build`, unlocked tiles plus one teaser tier,
actions returned rather than applied. Six spell families shipped; the rest are §10's tail.

- **Wisp** — a timed raise of the caster's own carried light: an ordinary entry in the GI light list wearing the
  `ConjuredLight` row's class, level and whitening answering to the caster's Arcana (`unlock::wisp_light`, brightening
  and whitening at `WISP_BRIGHT_AT` — R30's "level alone reads as more blue" lesson). Server debits and tracks expiry
  session-local (`wisp_until`); the buff deliberately drops on disconnect. Remotes see it through the snapshot fx byte
  (§8).
- **Kindle** — a standing orb of conjured light left in the cell the caster's HEAD occupies, never where they aim: the
  server validates the rung and that the cell hosts (`kindle::hosts` — anything a head fits in, water included), debits,
  and stands the light on the sim's own clock for `kindle::SECS`, telling everyone with `ServerMsg::Kindled`. The
  standing list is capped (`MAX_KINDLED`) at one light to a CELL — a recast refreshes rather than stacks — because only
  expiry takes an entry off a list every joiner is handed, and keying it to lit places is what makes the ceiling
  generous rather than a rationing of casts. **Not a block and not journaled**: it is one more entry in the client's GI
  light list wearing the `ConjuredLight` LUT row's class, plus a `teleport.wgsl` orb, so it leaves no edit behind, and
  its clock is its own — the caster can port away or leave the server and it burns out on schedule regardless (the sim's
  list is what a joiner is handed). Nothing is predicted: the caster learns of their own light from the same broadcast
  as everyone else. The earlier block-plus-sweep form tied the lifetime to the world's day and the caster's vicinity;
  `CastKind::Kindle { cell, face }` is that form's retired verb, decodable and refused.
- **The Sphere** — a one-cell-thick shell of the caster's HELD block closed about their head, and the first spell that
  WRITES cells. Every cell is derived on both ends from the head, the rank's radius and the block (`sphere::sets`: the
  shell is the interior's face-boundary, which makes it sealed against fluid and as thin as a sealed shell can be), so
  `CastKind::Sphere` carries only the hotbar slot — DECLARED, like an edit's, because the input path is jitter-buffered
  and the server's copy of the selection can lag the hand. The shell claims air and water alike (a wave left standing is
  a hole) and leaves terrain in place, so a sphere cast on the ground is a dome fused to it; interior water turns to
  air. All or nothing, the Arc's rule — a player in the shell or an unresident cell refuses the whole cast before any
  debit; a protected cell is simply not written, the way a fluid write is refused at the cell, so the sphere closes
  around a locked pad exactly as water would. **The shell is real matter, paid for**: the bill is
  `sphere::blocks_needed` of the held block — the shell's air and water cells, never the terrain it fuses to — plus the
  Unda fare plus the wisp's, taken together or not at all (`Inventory::try_take_fares`) as the channel begins, and
  SETTLED at its end: a cell the climb declined (protection, a player who stepped in) is refunded at the seal, and an
  unwind hands back the fares, the blocks never placed and every block it just took down — but not one somebody dug out
  mid-climb, since the dig already paid them. A dig of a standing shell hands the block back like any placed block,
  which is why the spell needs no conjured row of its own and why the world's own rules apply to what it builds: grass
  smothers under grass, a shell of dirt greens. What it may be built OF is `sphere::accepts`, DERIVED from the row
  rather than listed: a block the bar can hold, that a body collides with (`shapes::collides`, which is also why the
  torch is out), that is a plain block or a thing rather than a worked form (`ShapeFamily` — slabs, stairs and doors are
  oriented by their placer, and a new form is refused unnamed), and whose whole object at rest fits one cell by
  `shapes::object_bounds` — so a door fails on geometry, and a furnishing that grows a second cell fails the day it does
  — and the client's own gates speak the refusal by name (`Msg::SphereNoBlock`, `SphereBadBlock`, `SphereBlocksShort`,
  the last with the count) where the wire's `CastRefusal` reasons are spell-blind. **The seal lights the wisp**
  (`wisp_price`, `unlock::wisp_secs`), because most matter makes a dark room: the fare is part of the bill and its XP
  part of the award. Three ranks (`unlock::sphere_radius`), each a wider sphere at a doubled fare. **The cells never
  travel.** `sphere::plan` IS the shared rule — one head, one radius and one block give the same writes in the same
  order on both ends — so the wire carries only what identifies the sphere and how far it got:
  `ServerMsg::SphereWinding` as the channel begins and `ServerMsg::SphereEnded` at the seal or the break, two frames for
  a cast of any rank (plus a rebase when the settlement returns something). The per-cell broadcast this replaced sent
  one `EditApplied` per cell, hundreds inside a single tick against an outbound queue whose refusal DROPS the peer —
  which made a deliberately broken channel a free, repeatable mass disconnect. The server still journals every cell it
  writes, and that is what a joiner, the resync rotation and an evicted column all read, so nothing about arriving late
  changed. `landed` is a PLAN-PREFIX length (`sphere::landed_by`) and deliberately NOT a count of cells written: the two
  differ wherever a landing was declined, and a client cannot derive which ones the server skipped, so the prefix is the
  one currency both ends share — and it is what makes a break mid-climb exact. The closing is the SHELL, and it happens
  DURING the channel: the plan's order is floor to crown with each ring swept around, and the cells land in that order
  across `unlock::channel_ticks` — the caster's client and the offline engine through the same `sphere::Closing`, the
  server through its own write path — so the sphere rises about the caster while they stand still and seals overhead the
  tick the channel completes, with nothing animated but the world. EVERY watcher runs that same climb, not just the
  caster: `SphereWinding` names the head, the radius, the span and the block, which is the whole of what `sphere::plan`
  needs, so a remote's sphere rises in place rather than appearing whole — and `SphereEnded`'s prefix reconciles
  whichever clock ran ahead. An interrupted channel UNWINDS: the client sends `CastBreak` (and the server breaks it on
  its own if the caster moves or leaves), every cell still standing comes back down crown-first through the same write
  path, and the bill — taken as the channel began — returns. An unwind reads the climb's OWN record rather than
  re-deriving, since a plan drawn against a standing sphere names precisely the cells missing from it. `SphereEnded` is
  the SEAL as well: the one recording (`ward_close.ogg`, the file keeping the spell's old name) and the caster's wash; a
  cell landing makes no sound of its own. A cell somebody steps into mid-climb is left unwritten — a hole where a player
  stood, the protected cells' honesty — and a block somebody digs out mid-climb stays dug through an unwind. **The spell
  was the Ward** and built its shell of `Wardglass`, a `conjured` row that digs to nothing; the Causeway still writes
  that row, and a server boot sweeps every pane of it out of the journal (`sweep_conjured_glass`), since at boot nothing
  legitimately holds one — the old spheres are gone, and a road's plank left by a crash has no trail to retire it.
- **The Causeway** — a trail of conjured glass laid one plank at a time under the feet the tick they leave support and
  taken up behind: `causeway::TRAIL` planks stand at once and the head retires the tail, so the road follows the caster
  and never accumulates. `Trail::wants` is the whole rule and its only input is `Player::grounded` — a supported player
  spends nothing, which is why standing still costs no planks and why no velocity or facing enters an answer the two
  ends would have to share. `step_cell` puts the plank in the highest cell whose TOP lies at or below the feet, so it
  meets them rather than closing over them — which is also why the road is laid **per TICK and never per frame**
  (`a_road_is_level_at_every_frame_rate`): the resolver leaves the feet exactly on the top they walk off, so the tick
  support is lost is the one tick whose answer is level with the ground, and a tick later the feet have sagged and the
  answer is a whole cell lower. Asked once a frame, the road descends a block per plank at every rate but one tick per
  frame, which is precisely the rate every capture path runs at. A plank claims air and standing water exactly as a
  Sphere's shell does (`a_plank_claims_exactly_what_a_sphere_pane_claims`), which is what lets a road cross a lake. **A
  held jump climbs it**, and that is intended rather than tolerated (`a_held_jump_climbs_the_road`): the rule is "the
  feet have no support, so give them some", and a jump is the feet having no support. The only way to stop it would be a
  predicate telling a fall from a jump — per-tick movement state the two ends would have to agree about, which is the
  surface §1 exists to avoid — so the Causeway is priced as the access spell it is rather than as a safety net.

  **This is the one spell whose cells the caster's client NAMES.** The Sphere derives both ends from one head and one
  radius; a road cannot, because a plank is owed the tick the feet leave support and a round trip is several ticks —
  server-authoritative planks arrive after the fall. So a plank rides the standing edit machinery as
  `EditKind::CausewayStep`: predicted locally under a seq, BOUNDED server-side by `causeway::claim_ok` against the
  server's own copy of the caster's feet rather than re-derived, and reverted by the same rollback as a refused place.
  What makes that assertion safe is not the check but the trail — every plank laid retires another, so a crafted
  client's standing footprint is `TRAIL` cells of creditless, unslottable glass beside its own body whatever it claims,
  a smaller blast radius than a player with a stack of stone. Its token bucket (`CAUSEWAY_RATE_PER_SEC`) is its own and
  deliberately not the edit one, because a rate-limited plank is a FALL.

  **Retirement is deliberately NOT predicted.** The server owns the tail and its removals arrive as ordinary
  `EditApplied`, so the two ends can never hold different roads; the client's trail is a dedup list, not a schedule. A
  plank comes up only if it is still the glass the road laid — the Sphere unwind's rule, so a pane somebody dug stays
  dug — and only a plank actually WRITTEN joins the trail, which is what stops a road crossing a Sphere's shell and
  taking one of its panes with it. A departure takes the road up (`drop_client`), because a trail nobody holds can never
  retire itself — and so does a teleport (`retire_road`, the span left running), because a removal can only be written
  to a RESIDENT column and a road a world away is one nothing will ever be able to take up. A removal that still finds
  nowhere to go is the sim's debt rather than the trail's, held in `owed_planks` and retried until the column comes
  back: it is the one write here that has to LAND eventually rather than be skipped the way fluid's and the sweeps' are,
  since skipping it leaves permanent journaled glass with nothing left that could remove it.

  The row is a TOGGLE, not a channel to wait out: recasting takes the road up at once, since a caster who wants to
  descend cannot spend a minute being caught. No refund — the fare bought the span, not the seconds spent. Three rungs
  (`unlock::causeway_secs`), each lengthening the road against a fare that climbs by MORE
  (`a_longer_road_is_never_a_cheaper_second_of_one`). Three things can still drop you, each the server refusing a plank
  the client predicted: a protected cell — arrival regions reach the client only offline (§4), so this is the one it
  cannot predict and you fall out of the sky over a waystone — a column the server has not got resident, and a cell
  another player is standing in.

- **The Mason's Arc** — Arcana's answer to the Resonant Pick: the pick widens *breaking*, the Arc widens *placing*. A
  toggle in the book; while on, a plain placement extends along the verified normal to `unlock::arc_cells` cells under
  one seq (`EditKind::PlaceArc`), debiting one block per cell. The server re-derives the line from its own raycast (the
  pick's no-cheat-surface rule) and the line is **all or nothing** — a partial line would leave the two ends deriving
  different sets. The arc multiplies PLAIN placements only: slabs grow, doors pair and water pours, each its own
  machinery the line must not multiply. It pays Arcana for its REACH alone (`skills::arc_xp`): the cells past the first,
  at the smallest rate on the ladder — every cell is billed out of the player's own stock, so the ordinary place pays
  nothing and place-spam is no faucet.
- **Travel** — Return Home (§2) and the waystone roster (§3), each cast a channel with the same interruption surface.

## 6. Costs & XP — Arcana is the standing essence sink

- **One governor per spell.** Home is cooldown-governed and free forever; everything else is price-governed with no
  cooldown — the channel is feel and interruption surface, not a governor. Underneath all of them the server keeps a
  per-client token bucket (`CAST_BURST`/`CAST_RATE_PER_SEC`) spent by every `Cast` ATTEMPT, refused ones included: a
  sphere's validation derives a whole shell and scans every player against it BEFORE anything can be refused, so a
  bucket that charged only for success would leave that work unbounded. It is an abuse ceiling, not a pacing knob — a
  channel outlasts the whole burst, so no honest client reaches it, and running dry is silent.
- Waystone fares are a hand table (`alchemy::cast_price`) that climbs the ESSENCES with the ladder: the lowland ports in
  Vitae, then Terra, the cold and the shore in Unda, the mountain in Ferrum and the far cold in Aether — so Arcana's
  spine spends every faucet the other two ladders open rather than one, and the payability pin asks each essence's own
  faucet. The AMOUNT rises with remoteness only WITHIN an essence; across them it cannot, and the far cold's is the
  smallest figure in the table. The wisp prices in Vitae (`wisp_price` — what a fresh player's first heartwood draws
  pay, the day-one bootstrap, doubling with the burn at `WISP_BRIGHT_AT`); kindle in Terra (`kindle_price`, cheapening
  at `KINDLE_CHEAP_AT`); the sphere in Unda (`sphere_price`, doubling with each radius), the water essence for the spell
  that drives water out — shared with the cold and shore ports, so the one springstone faucet feeds both halves of the
  ladder — on top of the held blocks its shell is built of and the wisp fare its seal lights, one bill. The causeway
  prices in Unda too (`causeway_price`): the two cell-writing spells share one price line. The fares and the cast-XP
  rates are ONE decision, never two: raising a fare alone would pay more XP for costing more, which is the opposite of a
  sink. The payability pin asks the standing question: **a cast must be payable at the level that unlocks it.**
- **XP per priced cast derives from the price, weighted by the essence it drains.** `skills::cast_xp` takes the fare's
  essence as well as its amount and multiplies by `cast_xp_per_unit`, which answers `CAST_XP_PER_ESSENCE` for four of
  the five and `AETHER_CAST_WORTH` times that for Aether. A unit is not one worth across the ladder — Aether is the one
  essence nothing can transmute INTO, so what a single unit of it buys is a whole passage priced in anything else — and
  the unit-blind rate this replaced was a real regression the moment the fares started climbing: it paid the endgame far
  cold port the LEAST Arcana of any port in the roster. A price retune still moves its XP with it, and
  `the_port_tables_are_twins_and_the_fare_is_payable` sweeps the level-sorted roster to keep the award STRICTLY rising
  with remoteness, which is the property the amounts alone can no longer carry. Home derives from distance instead (§2),
  and the Arc pays its reach — the three shapes are pinned together.
- **The fares ARE Arcana's XP spine** — there is no other Arcana faucet but repeatable play: ports, returns, kindles and
  wisps, each paying what it drains (§3 records why the first-arrival milestone was withdrawn). Records written while
  milestones existed carry Arcana totals this ladder cannot produce, so the player store winds Arcana — and only Arcana
  — back to nothing on read, once, at `FIRST_VERSION_WITHOUT_ARRIVALS`.
- Every threshold lives in `unlock.rs` as always: the waystone rungs, `KINDLE_AT`, `ARC_AT`/`ARC_WIDE_AT`,
  `WARD_AT`/`WARD_WIDE_AT`/`WARD_VAST_AT`, `WISP_BRIGHT_AT`, `TELEPORT_SWIFT_AT`/`TELEPORT_SWIFTER_AT`,
  `CHANNEL_QUICK_AT`, `KINDLE_CHEAP_AT`, pinned by the Arcana vector in `unlock_levels_are_the_threshold_levels`.

## 7. The skill

`Skill` is `repr(u8)` append-only; Arcana is the third variant and every wire and save format already carried it.
Storage is XP-only totals, level always derived, the pinned integer curve untouched. The player record carries the
shared teleport tick, so a rejoin cannot reset the clock. The character screen's teaser machinery reads unlock tables
and therefore serves Arcana for free.

## 8. Wire inventory (v39)

- **The spawn spell needed no wire at all**: the channel is client pacing in front of `ClientMsg::ReturnToSpawn`.
- **`ClientMsg::Cast(CastKind)`** — shaped for the whole roster from day one (`Waystone(kind)`, `Wisp`,
  `Kindle { cell, face }`, v33's `KindleHere`, which carries nothing because the server already holds the head, and
  v39's `Sphere { slot }`, which carries the declared hotbar slot and nothing else, on the tag v34's `Ward` held) so it
  never needs a v2. A sphere's cells do not ride the wire at all:
  **`ServerMsg::SphereWinding { by, head, radius, span, block }`** and
  **`ServerMsg::SphereEnded { by, head, radius, landed, sealed, block }`** (both v39, on the tags v35's `WardWinding`
  and `WardEnded` held) are the whole of it, two frames per cast with both ends deriving the sphere from `sphere::plan`,
  and **`ClientMsg::CastBreak`** (v34) is the caster's word that the channel broke. Every one of those tag reuses is why
  `MIN_PROTOCOL` moved with the version, for both of the reasons a floor ever rises: a client that cannot decode them
  stands inside a sphere it never sees, and a tag whose meaning moved means an older peer would decode it *successfully*
  and read the wrong thing. `CastRefusal` grew `NoBlock`, `BadBlock` and `BlocksShort` (v39, appended).
  **`ServerMsg::Kindled { cell, secs_left }`** (v33) is every standing light, to everyone on the cast and to a joiner
  through the reserved join-sync pump BEHIND its journal columns — bursting the backlog inline could fail the join
  outright once enough lights stood. **`ServerMsg::Waystones`** carries the world's resolved sites in join sync (server
  state the client cannot derive — each eye was resolved against the journal once and stored).
  **`ServerMsg::CastRefused(CastRefusal)`** is a reason, never a sentence — `voice` chooses the words.
- **`EditKind::PlaceArc`** rides the standing edit request so craft-then-place ordering and slot declaration hold.
- **The Causeway's planks are ordinary edits.** `EditKind::CausewayStep` rides the standing `EditRequest`, so seq, ack,
  rollback and `pending_at` carry a plank with no new machinery and nothing else in the request is read;
  `CastKind::Causeway` begins the road and `ClientMsg::CausewayEnd` takes it up early, both carrying nothing to trust.
  All three are v36, and `MIN_PROTOCOL` moved with them for the FIRST of the two reasons a floor rises: a v35 server
  cannot decode the cast, so a newer client would offer a spellbook row that kills the session rather than casting.
- **Remote-visible casting** is one appended snapshot byte: `RemoteState.fx` (`pack_fx`/`unpack_fx` — kind, whitened,
  level), today carrying **the wisp only**, because a wisp is the one effect whose look depends on state not in the
  snapshot. A kind is reserved for the ore-draw's remote half (it needs the drawn cell, so it waits on its own design).
  Appending a struct field is the one genuinely breaking postcard change — this is what the v32 `MIN_PROTOCOL` raise
  carries.
- **A remote's teleport needs no wire at all**: the between-snapshot displacement no honest motion produces is detected
  client-side (`RemotePlayers::take_jumps` — a length floor well above the adoption envelope's corrections AND a speed
  floor above terminal fall), pops the remote to its destination instead of smearing it across the map, and fires the
  same departure/arrival bursts as your own jump through one shared site.

## 9. Hard problems, named before they bite

1. **Channels vs Delve hazards.** Once damage exists, a teleport channel is an escape rope. The rule is stated now so D3
   inherits it: taking damage interrupts a channel through the decay mold rather than the hard reset movement uses.
2. **One governor per spell** (§6) — any new spell picks one.
3. **Solo worlds.** Always-on protection applies offline too — one rule, and the arrival promise holds in every mode —
   but a solo player cannot build inside their own spawn sphere. `LOCK_RADIUS` is sized to the pad, not the
   neighbourhood, for exactly this reason; the pressure valve if it stings is a solo relocate-spawn action, not a
   protection toggle.
4. **Waystone density vs biome rarity.** One waystone per kind per world is the v1 answer; a second ring of more distant
   duplicates is a later unlock wave if the map outgrows it — derived the same way, so nothing about the wire or the
   store changes shape.
5. **Placeholder audio.** The cast loop and both teleport sounds currently reuse existing oggs; the wanted assets are
   named in `assets/sfx/SOURCES.md`.

## 10. What is NOT built

- **Conjured passage** — BUILT, as the Causeway (§5), and in the one shape that did not have to wait. What this
  described was bridge cells on a reversion clock, which needs DELVE-DESIGN D2's scheduled-cell-write substrate and
  still does. Both shipped spells sidestep the clock instead, from opposite sides: the Sphere's shell is PERMANENT
  journaled matter — the caster's own blocks, paid for and dug back like any placed block — and the Causeway's glass is
  retired by the plank that replaces it, so in neither case does a cell need a timer of its own. Kindle sidestepped it a
  third way by never being a cell at all.
- **Prospect pulse** — the passive audio senses recast as an on-demand ping. Audio only; the no-ESP stance holds.
- **Mending** — waits on D3 health.
- **Teleport-to-friend** — consent required, addressed by account id and never by name (the `--admin` lesson).
- **The personal waystone** — craftable inside your own claim, a serious essence sink. Waits on SOCIAL-DESIGN claims.
  This is where the exploration itch the rejected attune model scratched comes back, priced instead of walked.
- **Sanctum ports** — the Aether-priced deep destinations, tail content with Delve.
- **The remote ore-draw fx kind** — reserved in the fx byte, unbuilt (needs the drawn cell).
- No combat magic, no movement-modifying spells, no RNG, no mana bar, no instanced or private destinations, no
  visit-to-attune, no mounts. Health, damage and danger stay DELVE-DESIGN's.
