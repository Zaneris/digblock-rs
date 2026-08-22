# Arcana — travel, waystones & the arcane skill

Status: **BUILT** (branch `feat/arcana`, stages 1–8). This document came out of the 2026-08-21 design conversation on
new skills and is now the record of what shipped and why, plus §9's open questions and §10's unbuilt tail. The owner's
brief: turn return-to-spawn into a real spell with real teleport effects; unlock teleports to other biomes at higher
levels — to places the caster has genuinely never been, shared so friends who unlock them converge on the same spot;
group further spells under the skill; and make every teleport destination protected ground. Where this document names a
constant or function, the reference is to shipped code.

Decisions settled during the build, so they stay settled: the skill is named **Arcana** (`Skill::Arcana`); casting lives
in a **spellbook screen** (J; H starts the home channel directly, and the pause menu's old "Return to Spawn" row now
opens the book); **the spawn lock stopped being an operator toggle and is always on**, its region a **sphere**
containing the old box, and the same protection covers every teleport destination; waystone **structures are STAMPED by
the server as ordinary journaled edits, never worldgen** (§3); and the teleport visuals are their own
helix-and-column-of-light shaders, deliberately **not the confetti look**.

## 1. Goals and non-negotiables

Arcana is the travel-and-utility skill: Mining breaks blocks, Alchemy transmutes them, **Arcana moves things that aren't
blocks — the player, and light.** The constitution binds as everywhere else:

- **No RNG.** Destinations, costs and effects are pure functions; the one "discovery" in the design — arriving somewhere
  you have never walked — is a threshold unlock, the reveal-as-event grammar, not a roll.
- **XP is server-computed.** One new chokepoint (`process_cast`, the `process_alchemy` twin) plus the first-arrival
  milestones; offline arms award through the same core functions in the offline branches only.
- **No combat spells.** The surface stays exactly as safe as it was; danger belongs to DELVE-DESIGN.
- **No spell touches per-tick movement.** Levitation, speed, water-walking — each would run through `Player::tick` and
  the adoption envelope, making every such spell its own anti-cheat surface. Teleports dodge the problem entirely: a
  teleport is a server-side position SET (`Sim::teleport_client`), corrected by nothing because the server said so
  first.
- **Essence is the mana.** No mana bar, no new currency: casts are priced in essence, which makes Arcana the standing
  essence *sink* the economy explicitly wants. No cast creates matter — the one spell that writes cells writes conjured
  light that the wisplight sweep reverts (§5), and the one that writes real blocks (the Mason's Arc) spends the player's
  own inventory.
- **Web is the priority platform.** Effects are instanced world-space emitters in the additive water-pass slot — no new
  passes, no new render machinery, WGSL that passes real Chrome's front end.

## 2. The spawn spell — Return Home

Return-to-spawn kept its machinery (`ClientMsg::ReturnToSpawn`, `Sim::handle_return_to_spawn`, `teleport_client`) and
became a spell in front of it:

- **Level 1 grants it, which means everyone keeps it.** The spell is a held, interruptible channel
  (`unlock::channel_ticks`, shortening at `CHANNEL_QUICK_AT`) the client paces before sending the existing message — the
  server's cooldown remains the real gate. Movement, opening a screen or starting a dig RESETS the channel
  (`Msg::CastInterrupted`); the decay mold is reserved for D3 damage.
- **The cooldown steps down** at the Swift Return rungs (`unlock::return_cooldown_secs`; the level-1 answer IS the old
  flat constant, aliased rather than copied), read per-caster by the server.
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
  first cast is the discovery. First arrival at each waystone pays milestone XP once per account
  (`skills::waystone_milestone`, `visited_waystones` persisted with the player record). Because destinations are
  canonical, two friends who cross the threshold hold the *same* place — "meet me at the mountain waystone" is a
  sentence that works, and waystones are the game's first gathering points away from spawn.
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
actions returned rather than applied. Four spell families shipped; the rest are §10's tail.

- **Wisp** — a timed raise of the caster's own carried light: an ordinary entry in the GI light list wearing the
  Wisplight row's class, level and whitening answering to the caster's Arcana (`unlock::wisp_light`, brightening and
  whitening at `WISP_BRIGHT_AT` — R30's "level alone reads as more blue" lesson). Server debits and tracks expiry
  session-local (`wisp_until`); the buff deliberately drops on disconnect. Remotes see it through the snapshot fx byte
  (§8).
- **Kindle** — conjured light placed in the world: the server validates an AIR cell on an aimed face within honest
  reach, debits, and writes a `Wisplight` block as an ordinary journaled edit. **Expiry rides the worldjob scheduler** —
  the wisplight sweep (Wisplight → Air) both arms register, so lifetime is "until the sweep", offline parity is free,
  and quitting before expiry cannot keep a free lamp. Wisplight is conjured light, never matter: not slottable, drops
  nothing, no collision, and not a node host.
- **The Mason's Arc** — Arcana's answer to the Resonant Pick: the pick widens *breaking*, the Arc widens *placing*. A
  toggle in the book; while on, a plain placement extends along the verified normal to `unlock::arc_cells` cells under
  one seq (`EditKind::PlaceArc`), debiting one block per cell. The server re-derives the line from its own raycast (the
  pick's no-cheat-surface rule) and the line is **all or nothing** — a partial line would leave the two ends deriving
  different sets. The arc multiplies PLAIN placements only: slabs grow, doors pair and water pours, each its own
  machinery the line must not multiply. No XP by design — placement paying XP would make place-spam a faucet.
- **Travel** — Return Home (§2) and the waystone roster (§3), each cast a channel with the same interruption surface.

## 6. Costs & XP — Arcana is the standing essence sink

- **One governor per spell.** Home is cooldown-governed and free forever; everything else is price-governed with no
  cooldown — the channel is feel and interruption surface, not a governor.
- Waystone fares are a hand table in Terra (`alchemy::cast_price`), rising with the ladder; the wisp prices in Vitae
  (`wisp_price` — what a fresh player's first heartwood draws pay, the day-one bootstrap); kindle in Terra
  (`kindle_price`, cheapening at `KINDLE_CHEAP_AT`). The payability pin asks the standing question: **a cast must be
  payable at the level that unlocks it.**
- **XP per priced cast derives from the price** (`skills::cast_xp` = `CAST_XP_PER_ESSENCE` × the fare), so a price
  retune moves its XP with it and no spell pays out of proportion to what it drains. Home derives from distance instead
  (§2), and the Arc pays zero — the three shapes are pinned together.
- **First-arrival milestones** (`skills::waystone_milestone`, once per account) are Arcana's XP spine the way reveals
  are Mining's — each unlock lands with a milestone worth several levels at that point, without dwarfing repeatable
  play.
- Every threshold lives in `unlock.rs` as always: the waystone rungs, `KINDLE_AT`, `ARC_AT`/`ARC_WIDE_AT`,
  `WISP_BRIGHT_AT`, `RETURN_SWIFT_AT`/`RETURN_SWIFTER_AT`, `CHANNEL_QUICK_AT`, `KINDLE_CHEAP_AT`, pinned by the Arcana
  vector in `unlock_levels_are_the_threshold_levels`.

## 7. The skill

`Skill` is `repr(u8)` append-only; Arcana is the third variant and every wire and save format already carried it.
Storage is XP-only totals, level always derived, the pinned integer curve untouched. `visited_waystones` rides the
player record (format-version bump, read-compat, union on merge). The character screen's teaser machinery reads unlock
tables and therefore serves Arcana for free.

## 8. Wire inventory (v32)

- **The spawn spell needed no wire at all**: the channel is client pacing in front of `ClientMsg::ReturnToSpawn`.
- **`ClientMsg::Cast(CastKind)`** — shaped for the whole roster from day one (`Waystone(kind)`, `Wisp`,
  `Kindle { cell, face }`) so it never needs a v2. **`ServerMsg::Waystones`** carries the world's resolved sites in join
  sync (server state the client cannot derive — each eye was resolved against the journal once and stored).
  **`ServerMsg::CastRefused(CastRefusal)`** is a reason, never a sentence — `voice` chooses the words.
- **`EditKind::PlaceArc`** rides the standing edit request so craft-then-place ordering and slot declaration hold.
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

- **Conjured passage** — temporary bridge cells on a reversion clock. Waits on DELVE-DESIGN D2's scheduled-cell-write
  substrate; kindle deliberately did NOT build it (the wisplight sweep is a batch rule, not a timer).
- **Prospect pulse** — the passive audio senses recast as an on-demand ping. Audio only; the no-ESP stance holds.
- **Mending** — waits on D3 health.
- **Teleport-to-friend** — consent required, addressed by account id and never by name (the `--admin` lesson).
- **The personal waystone** — craftable inside your own claim, a serious essence sink. Waits on SOCIAL-DESIGN claims.
  This is where the exploration itch the rejected attune model scratched comes back, priced instead of walked.
- **Sanctum ports** — the Aether-priced deep destinations, tail content with Delve.
- **The remote ore-draw fx kind** — reserved in the fx byte, unbuilt (needs the drawn cell).
- No combat magic, no movement-modifying spells, no RNG, no mana bar, no instanced or private destinations, no
  visit-to-attune, no mounts. Health, damage and danger stay DELVE-DESIGN's.
