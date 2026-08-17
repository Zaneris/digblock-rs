# Delving — dungeons, hazards & the third skill (design document)

Status: **DESIGN ONLY — nothing in this document is built.** It came out of the 2026-08-16 design review
(SKILLS-DESIGN.md §13) as the answer to two findings at once: the world has no *place* — caves are noise, not
destinations — and the game has no system that pushes back. The owner's brief: underground dungeons whose blocks
regenerate so the dungeon resets itself over time, thought through before anything is built. This document is that
thinking. Where it names a constant or a function that does not exist yet, the name is a proposal; where it names one
that does (`ore_at`, `JournalIndex::record`, `protect.rs`), the reference is to the shipped code and the claim about it
has been checked.

The design is staged (§8) so each slice ships alone: ruins first (pure exploration content, no new mechanics), the
self-healing second, danger and the skill third. Mobs are deliberately last and deliberately unscoped (§4.4).

## 1. The reset model: regeneration is edits, not un-edits

The trap in "regenerative blocks" is imagining cells *returning to pristine*. That is unshippable here, and knowing why
shapes everything else:

- `JournalIndex::record` only ever inserts — "has this cell ever been written" is **monotone**, and the ore anti-dupe is
  load-bearing on that monotonicity (ore manifests only in never-edited cells; a cleared edited-bit would let a spent
  vein regrow).
- The mason's economy reads the same bit from the other side (`crumbles` vs an edited cell handing itself back).
- The client's `EditOverlay` mirrors the same insert-only rule, and a divergence between the two ends is permanent by
  construction.

So a dungeon block that "resets" is **the server writing the cell's derived value back as an ordinary journaled edit**.
The cell stays edited forever — and inside a dungeon that is *fine*, because two pins make the edited-bit locally
meaningless:

- **Pin A — ore never manifests in a dungeon region.** `ore_at` gains one early-out: inside `dungeon_region` (a pure
  predicate, §2) it answers nothing. Both ends already call `ore_at` identically, so the exclusion is deterministic
  everywhere, and the edited-bit inside a dungeon stops carrying ore meaning entirely.
- **Pin B — no dungeon fabric block reads the edited bit.** Dungeon rows use unconditional `drops` (or none), never
  `crumbles`, so the conditional-crumble economy never fires on them either.

With A and B, the monotone rule is untouched globally and inert locally. No journal migration, no second overlay, no
wire change for world data; join sync stays seed + edits; boot compaction is per-cell LWW, so an eternally-cycling cell
costs one journal entry, not unbounded growth. And because the whole thing is derived, **existing worlds grow dungeons
retroactively, exactly as they grew ore**.

The engine already contains this pattern's precedent, twice. Fluid is "the world heals itself" shipped: a scooped pool
regenerates via a server sim rule, and the healing is broadcast as ordinary edits that land in the journal like any
other. And compaction already makes cyclic rewrites cheap. The Warden (§3) is fluid's mold applied to stone.

**Rejected alternatives, recorded so they stay rejected:**

- *An expiring-edit overlay scoped to dungeon regions.* A second source of truth beside the journal: it breaks "join
  sync = seed + edits", needs its own persistence and client mirror, and an expiry clock both ends must agree on.
  Everything it buys, journaled resets buy cheaper.
- *Instanced or phased dungeons.* Cleanest in the abstract; a new server concept (per-party world state), a new
  streaming path, new wire — and it betrays the game's identity, one persistent shared world where the player beside you
  is real. RuneScape's own Dungeoneering carries the warning: instanced content read as "a minigame, not the world".
  Declined for v1; revisit only if contested resets prove socially toxic (§7).

## 2. Generation: the apron pattern goes underground

Dungeons generate exactly like surface structures and ore: `dungeon_region` is a salted position hash over a coarse 3D
grid — `structures_near`'s apron pattern, which `worldgen/ore.rs` has already proven at depth. A dungeon is a
deterministic room-and-corridor complex authored as a pure function of its anchor cell: rooms, corridors, hazard
placements and reward-node cells all derive from the anchor hash, so every chunk within the apron computes identical
results independently, complexes span chunk borders with no pipeline state, and `build_column` returns the bytes it
always returned (the dungeon, like ore, is stamped downstream of worldgen — see §8's D1 for the mechanism decision).
Depth-banded like the ore tiers, and overlapping for the same reason ore bands overlap.

One constraint ore did not have: a complex spans dozens of blocks, so the apron radius is much larger than the deposit
grid's. That is a knob, not a design problem — the pattern tolerates any radius; it costs candidate evaluation per
chunk, which gets a measured pass under BENCH.md discipline before the radius is chosen.

Worth saying plainly: **the renderer was accidentally built for this feature.** Unproven daylight fades with depth, so
deep places are honestly dark; carried lights, torch NEE, and ore-seam surface emitters are the lighting program's
hardest shipped cases, and every one of them is an underground room lit by carried and placed sources. A torch-lit ruin
at the bottom of a cave is the payoff content for two design documents' worth of engineering, which is why D1 ships
first and alone.

## 3. Dungeon fabric and the Warden

Dungeon walls and floors are new block rows (working name: the **Wardstone** family) — very hard, diggable, dropping
nothing — watched by the **Warden**, a server sim system in fluid's mold: every player edit inside a dungeon region
schedules the cell for reset after a deterministic delay, staggered by position hash so a breached wall heals
cell-by-cell rather than snapping back as a slab. A reset defers while a player occupies the cell (the
`overlaps_solid`/InsidePlayer machinery place-validation already has — the same shape as protection refusing the fluid
*write*).

This keeps the voxel promise while making dungeons the one place the world pushes back: you *can* tunnel past a puzzle —
expensive, temporary, and the wall closing behind you is the fantasy. It is also the honest reading of "the dungeon
resets itself": resets are per-cell timers hung off player edits, so an untouched dungeon costs zero sim — fluid's
costs-nothing-at-rest property, kept.

Offline parity holds by construction: the Warden lives in core and runs in the offline sim arm as fluid does. It writes
world edits, not XP, so no new award chokepoint exists.

## 4. Danger without RNG

### 4.1 Hazards, not monsters

V1 combat is **the dungeon itself fighting you**: deterministic, telegraphed, world-clock hazards — pure functions of
(position, seed, world tick), the no-RNG philosophy applied to danger. Koster's frame is the argument: fun is mastering
learnable patterns, RNG combat obscures the pattern behind rolls, and deterministic telegraphs are the purer form — the
one form consistent with "no RNG anywhere".

- **Pulse hazards**: vent blocks that kindle (an emissive ramp — the lighting engine makes the telegraph gorgeous and
  legible for free) and then fire a damage pulse over derived cells, on a position-hashed phase of the world clock. A
  learnable rhythm with generous windows — seconds, not frames: a server-authoritative sim over browser RTTs cannot host
  twitch dodging, and this is chess-with-lava, not a reflex test.
- **Drain fields**: standing in one ticks health down; corridors you route around, sprint, or still.
- **Attunement — the key new verb, built entirely from the dig-hold.** Point at a hazard node and hold: it is a
  `DigState` hold with decay, client-predicted and server-verified through the exact machinery `verify_digs` already
  runs, and completion *stills the hazard for a deterministic window* instead of breaking a block. The siphon proved
  this mold (a hold that extracts rather than breaks); attunement is a hold that pacifies. XP awards at the existing
  dig-credit chokepoint — no new chokepoint, no new anti-cheat surface.

### 4.2 Health

A small server-owned pool, damaged **only** by hazards — nothing else in the game hurts anyone, and it stays that way —
regenerating deterministically over ticks outside dungeon regions. It reaches the client as an idempotent total in the
`XpGain` mold (reorder-safe; the client animates the delta). Not client-predicted: damage is slow and rare enough that
snapshot correction is invisible.

**Hitpoints is not a skill.** A ladder with one XP source is a checkbox skill; the pool derives from the Delving level
instead, the way everything else derives from tables. If a later era gives health many sources, revisit.

### 4.3 Death

The run is the stake. No item drops exist to lose, XP loss would violate the spirit of "a dig's pay is fixed for life",
and an essence toll lands hardest on exactly the new delvers who have nothing. Zeroed health ejects you to the dungeon
mouth — the Return-to-Spawn machinery, retargeted — with the Warden healed behind you and your consumed wards spent.
Progress-as-stake is RS Dungeoneering's model and every roguelite's; it needs zero new economy machinery and cannot be
griefed into a negative-sum outcome.

### 4.4 Mobs, honestly

Mobs are the most expensive, lowest-confidence item in the whole roadmap: server-authoritative entities mean pathing, a
new snapshot section beside `RemoteState`, interpolation, animation, art, audio and AI tuning — and the web client's
frame budget is already carefully spent. Nothing else here risks shipping janky the way mobs do, and DigBlock's verb set
is entirely world-directed. So: **not scheduled.** If the hazard game's ceiling is genuinely reached, the minimal
creature is a *stationary warden* — a block-cluster boss with patterned, escalating area pulses, rendered through the
players/held-geometry mold: no pathing, one snapshot field. Roaming mobs are a separate argument for a separate day.

## 5. The Delving skill

One new skill wraps the feature — RS Dungeoneering's precedent: the dungeon *is* the skill. `Skill` is `repr(u8)`
append-only, so the enum grows by one and every wire and save format already carries it.

- **XP sources**: attunement holds (the dig-credit chokepoint), first-reach depth/sanctum milestones per reset epoch
  (server-side, same chokepoint family), and reward-node draws (§6).
- **Gates** (all `unlock.rs` tables, nothing new in kind): which dungeon tiers admit you (a `dig_blocked`-style refusal
  at the threshold, in the `RejectReason::SkillGated` mold), which hazard tiers you can attune, the health pool's size,
  and a Delving line of catalog cosmetics ending in a capstone held item (SOCIAL-DESIGN.md's mastery-item pattern).
- **Placement**: per SKILLS-DESIGN §13, launch content lives in levels 1–60 with the tail reserved for expansion.

## 6. Rewards and the Aether governor

**Essence, never items.** Reward nodes are vein-like derived cells inside the dungeon that reset on the dungeon's epoch
— siphon-drawn, paying through `field_yield`'s exact mold, credited via the standing `InventoryFull` rebase. (The
`/give` lesson, reapplied: essence rides existing machinery; an item reward would be a world edit with slot
reconciliation behind it.)

Deep-tier nodes carry a small fixed Aether line on a long deterministic epoch, behind travel, hazard and Delving gates.
That makes dungeons the game's second Aether faucet — **with a governor**:

- This does not touch "no matter creation": that invariant forbids closed *player* loops (bench-side), and world faucets
  are how all matter enters — ore is already an unbounded faucet over an unbounded world. What a second faucet must not
  do is undercut the first: node Aether-per-hour stays below a comparable Aetherock session, tuned with the same
  instrumentation SKILLS-DESIGN §13.4 sets up.
- Delving also *consumes* essence: wards and attunement charges are hand-priced consumables (§7.3 pricing, the
  payability pin applies), so the loop is an essence cycler with a thin Aether margin — exactly what the standing Aether
  sinks need to breathe.
- The ward is the game's first consumable that is neither a block placement nor a craft-to-own. Cheapest shape: the
  bucket-charge precedent — a capped count in the inventory, debited server-side at the attunement gate. No new
  persistence, no new wire beyond a reject reason.

## 7. Hard problems, named before they bite

1. **Reset epochs are shared state.** First-come-takes-the-node per epoch is contested — RS-flavored resource
   competition, fine for v1 — but on a busy server it starves latecomers *deterministically* (no roll to save you). If
   that sours, the fix is per-player node cooldowns riding the `player_store` record: real new state, so earn it with
   evidence first.
2. **The region predicate is version-locked to the journal.** `dungeon_region` changing shape moves which cells the
   Warden owns and where Pin A excludes ore — it rides the shared worldgen/journal `VERSION` like any worldgen retune,
   and the predicate gets a golden pin.
3. **Protection vs the Warden.** Player claims (SOCIAL-DESIGN.md) and `/set-spawn` are refused inside dungeon regions —
   one predicate check at claim time — or protect.rs and the Warden fight over the same cell.
4. **Damage fairness under prediction.** A player who dodged on their screen but not on the server's would eat a pulse
   they visibly avoided. Telegraph windows an order of magnitude above RTT, and pulse edges that err in the player's
   favor — the `dig_grace` philosophy: every seam errs toward the honest client.
5. **Warden write volume.** A griefer strip-mining a dungeon schedules a burst of resets; the Warden takes a per-region
   write budget (the `STREAM_BUDGET_MS` philosophy applied to sim edits) so healing can never flood the snapshot stream.

## 8. Staging

Each slice is worth shipping alone, and each builds substrate the next one stands on:

- **D1 — Ruins** (small-medium): generation only. `dungeon_region`, authored complexes, Pin A's ore exclusion, hard
  fabric rows. No Warden, no health. Caves today are noise; an authored torch-lit ruin is a *destination*, and the
  renderer's showcase. The one open mechanism decision: whether the stamp rides the mesher's input like the ore reveal,
  or the fabric is written at first-visit like decoration's trunk roots — D1's first task is to settle that against the
  anti-dupe the same way the reveal stamp was settled.
- **D2 — The Warden** (medium): journaled resets, diggable-but-healing fabric, defer-on-occupied, the write budget.
  Dungeons stop being permanently strip-minable — and the scheduled-cell-writes service this builds is the same
  substrate a future Cultivation skill's crop growth needs, which is why D2 precedes any farming talk.
- **D3 — Danger + Delving** (large): health, pulse and drain hazards, the attunement verb, death-eject, the Delving
  skill, reward nodes and the Aether governor. The headline release.
- **D4 — the stationary warden boss** (unscoped): only if D3's ceiling is genuinely reached. Not scheduled.

## 9. What is NOT in this design

No roaming mobs, no pathing, no PvP, no hunger, no fall damage, no item drops on death, no instancing, no second-overlay
world state, and no Aether-*creating* craft anywhere — "Aether cannot be transmuted into, ever" survives this document
untouched. Health exists only where hazards do; the surface world stays exactly as safe as it is today.
