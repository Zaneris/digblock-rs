# Skills, Essence & Ore — design document

Status: **§11's M1–M7 are BUILT and merged. The document below is the design as written; where the implementation
diverged, the code and CLAUDE.md are the authority and the divergences are listed here.** The 2026-08-16 design review
then **compressed both content ladders into levels 1–60** (§13, which supersedes §4.3's and §5.5's ladder positions
wherever they disagree — the thresholds themselves live in `unlock.rs` and `alchemy.rs` as always). The 2026-08-18
essence redesign (**§14 — essence is found, not ground**) then REMOVED bench extraction outright and rebuilt the craft
economy around nodes, feedstock and catalysts; §3.1–§3.3, §4.1's extraction rows, §4.2's closure argument and §8.1's
EXTRACT tab are superseded wherever they disagree with §14, and are kept as the record of what the redesign replaced.

**Done:** M1 skills skeleton · M2 essence economy + the two-tab alchemy screen · M3 level-scaled dig speed · M4 tool
slot + Stage-A emissive + Torch · the RTGI rewrite (GI-DESIGN.md replaced LIGHT-DESIGN.md's Stage-B voxel lighting
entirely) · **M5 ore** (`worldgen/ore.rs`, the reveal stamp, the Essence Siphon, field extraction, dual XP) · **M6 the
Resonant Pick** + prospector's sense + the mastery line at the cap · **M7's first wave** (§7.2's material × shape GRID,
three timbers, furniture, dressed stone, glass, metal, crystal, the two lamps, the sigil).

**Where the code diverges from this document, and why:**

- **§7.2's grid is the implementation, not a curation rule.** `unlock::craft` is DERIVED as `max(shape, material)` from
  `core/material.rs` and `block::ShapeFamily`; craft cost and extraction yield prorate the material's whole-block
  essence split by the shape's volume. The BLOCKS shape sits at the bottom of the shape axis, so a material arrives in
  every form you have already learned. Several thresholds moved and §4.3 is stale where it disagrees: the plain cubes of
  Wood, Dirt and Sand came *down* to the BLOCKS shape, and the Stone slab and stairs went *up* to Stone's own material
  level.
- **§5.3's depth bands OVERLAP.** Exclusive bands would leave a Mining-10 player at y = 20 finding nothing at all — past
  the shallowest tier's floor, and nowhere near the level the next tier reveals at; every tier stays available all the
  way down and the deep ones are simply rarer.
- **Ore ids are not the ones §5.3 reserved** — the lumber set took those. Ids come from the append-only `blocks!` table
  and nowhere else.
- **The reveal stamps block IDS into the mesh input**, not texture layers — the mesher, the emissive bit and
  `block_tint` then need no changes at all. The mining level reaches the worker in an `OreStamp` rather than as a mesher
  field.
- **Multiplayer keeps an `EditOverlay`** (only re-application is conditional), because the anti-dupe's "has this cell
  been written?" question is one the client has to answer too.
- **Field extraction needs no new wire**; only the tool refusal does (`RejectReason::NeedsTool`, protocol v21).
- **The pick is a PLANE, not a bore** — it breaks the plane of the face it hits and widens once more at its later Mining
  threshold (`unlock::pick_cells`) — and its timing multiplier is LEVEL-only, never derived from how many cells qualify,
  or the two ends disagree by the whole width of the spread over a single differing cell.
- **Furniture and the bed are FUNCTIONAL items**, not grid cells: a table is a thing, not a form any substance takes.
  The bed is one cell (a horizontal two-cell pair is new machinery — the door's pair is vertical), and its Flora + Aqua
  price is the catalog's first genuinely cross-family cost.
- **Glass ships opaque** (§12's render question is unanswered: the depth prepass writes the whole quad, so a cutout
  discard needs the prepass to sample the atlas).
- **Ore's bench XP derives from its extraction base like everything else** (`skills::convert_xp`), not §4.1's hand-set
  per-tier figures — the rule was kept, the exception dropped. Field XP (§5.4) is tabled as written.

**Still design:** §7.2's later SHAPES (fence, ladder, pillar, pane, gate, trapdoor — each a `shapes.rs` project), the
lossy essence exchange and the Water craft's own `AlchemyOp` arm, the ore-draw effect's remote half (a snapshot field
for who is drawing on which cell) and its sustained audio loop, the prospector's own sourced shimmer, and §12's open
questions.

Milestone detail as originally built: M1: `digblock-core/src/skills.rs` (curve + awards, golden-pinned), protocol v16
(`Skills`/`XpGain`), `inventories.bin` v4, `state.json` skills, both server chokepoints awarding, XP toasts, the
character screen (C / pause row / `DIG_SCREEN=skills`), offline parity through the same core functions. M2:
`digblock-core/src/alchemy.rs` (extraction / waste / bonus tables, all integer math, `no_level_ever_creates_matter`
pinned), the five essences with **legacy-preserving indices** (Flora keeps sub-wood's and Earth keeps sub-stone's — the
entire "decode-time migration" is that numbering choice, see §9), the Rubble rule, level-aware `apply_alchemy`, the
two-tab screen with the drag-to-slot drip (`DIG_SCREEN=alchemy|transmute`), and the server extraction bucket
(`extraction_pace_is_server_enforced`). Two design bugs §3.3 had were caught in implementation and fixed structurally —
see the bonus-line pump rule there. M3 (level-scaled dig speed, SIM v11, retuned end to end at v18) is landed, along
with a rework of the transmute tab into §8.1's family model. M4 is HALF landed: the Stage-A emissive bit (a bit of the
solid vertex's material word; chunk.wgsl returns fogged ink and skips sun/shadow/AO) and the Torch (ShapeKind::Torch —
post + glowing head, hand-priced per §7.3, the LIGHTS family on the transmute tab, in the DIG_SHAPES showcase). M4's
other half — §6's tool formalization — is landed too: tools are item-only `blocks!` rows (`Bucket`, `is_tool`, and at
the time `always_owned` — §13.5 has since made the bucket a craft like every other tool, and the flag went with it), the
tool slot is PINNED to the bar's end (legacy layouts that parked it mid-bar migrate on decode), `Hotbar::active_tool` is
the second axis riding the Layout wire's last entry (`WIRE_TOOL` = the bucket), tap-9 on the selected tool slot cycles
the owned roster, the faded rail draws once a second tool exists, and the server's Collect/pour gates check the active
tool. A consolidation pass (2026-08-10) then landed **Stage-B voxel lighting** (LIGHT-DESIGN.md — torches illuminate,
caves and roofed rooms go dark; §7.4 updated) and reconciled this document with the shipped code (§3.3 cumulative note,
§4.1/§4.2 per-line-ceil ideals, §4.3's late rows, §5.3's ore ids, §10 pump-rule pin), then landed §5.1's **per-material
dig families and the strength wall** (SIM v12, protocol v17; both retuned at SIM v18 — the curve rebuilt end to end and
the wall re-keyed onto the dig family), and its Alchemy twin, the **craft gate** (SIM v13, protocol v19 — a craft-only
block refuses to be dug below the Alchemy level that makes it, so a pickup can't route around an unlock; the
planed-timber set moved down to the joinery material's own level with it, joining the bucket charge). M5, M6 and M7's
first wave landed after that — the header above lists what each one brought. Originally fleshed out from the brainstorm
of 2026-08-09, grounded in the code as it stood (protocol v15, SIM v10, `inventories.bin` v3). Every value in here is a
tuning proposal, not a promise — and the values themselves now live only in the code that defines them (`skills.rs`,
`alchemy.rs`, `unlock.rs`, `material.rs`) — but the *shapes* (which functions exist, where authority lives, what
migrates how) are meant to be the real plan.

---

## 1. Goals and non-negotiables

Two launch skills — **Mining** and **Alchemy** — from level one to `skills::MAX_LEVEL`, RuneScape-flavored: slow
exponential curve, threshold unlocks that feel like events, XP from doing the thing.

Design rules that bind everything below:

- **No RNG anywhere.** Yields, ore placement, XP are all pure functions. Offline local worlds run the identical rules
  through `apply_alchemy`/the dig path, and every economy invariant in this repo is pinned by deterministic tests.
  "Chance of bonus" mechanics are replaced by threshold unlocks, which are the better feel anyway.
- **XP is server-computed, never client-claimed.** Awards happen at the two existing server chokepoints (the dig-credit
  block in `edits.rs` and `process_alchemy`). The client learns its XP over the reliable stream and derives levels
  locally from the shared curve.
- **No matter creation.** Extraction never returns more than a block's base, transmutation never costs less than the
  ideal, and a block's total extraction base is strictly less than its craft cost at every level. The old exact-closure
  economy — a block's worth of slabs extracting back into exactly one block — is deliberately retired, lossiness scaled
  by level *is* the progression, and monotonicity pins replace the closure.
- **Ore is derived, never stored.** Chunk data, the journal, and the wire keep carrying Stone. No worldgen version bump,
  no journal migration, golden hashes untouched, existing worlds grow ore retroactively.
- **Reuse the count-based inventory for every new currency** (the water-charge precedent): essences are flagged-id
  counts, so prediction, `inv_delta` rollback, `InventoryFull` rebase, `player_store`, and `state.json` all carry them
  with zero new machinery.

---

## 2. The XP curve (shared by all skills)

The cost of advancing from a level to the next is a **slow exponential**: a fixed leading coefficient against a fixed
per-level growth ratio, so the price doubles over a fixed span of levels and the whole curve is tuned by that one
coefficient. It lives as a **pinned integer table** rather than a `powf` — `skills::xp_to_next` / `total_xp_for` /
`level_for_xp`, golden-pinned by `xp_curve_table_is_pinned` — for the same reason the dig taper is one: it is a number
both ends of the wire must agree on to the unit, and a float would let two platforms round a level apart.

The shape that buys is RuneScape's: the middle of the ladder costs almost nothing against the total, and the last
stretch carries most of it — the "halfway point sits near the top" feel. It is softened a little here, so the halfway
mark sits further below the cap than RuneScape's does. At the endgame XP rates below, mastering Mining is a long-haul
goal measured in sessions rather than evenings, and Alchemy rides along beneath it if you process everything you mine.

Storage is **XP only** (`u64` per skill); level is always derived. `Skill` is a `repr(u8)` enum (`Mining` first,
`Alchemy` second, append-only) so persistence and wire carry `Vec<(u8, u64)>` and unknown skills from the future decode
harmlessly.

---

## 3. Essences

Five, replacing the two `Material` sub-material families outright:

| essence    | flavor        | primary sources                                                                      |
| ---------- | ------------- | ------------------------------------------------------------------------------------ |
| **Earth**  | mineral bulk  | stone, sandstone, gravel, sand, dirt                                                 |
| **Flora**  | living matter | wood, grass tops, leaves                                                             |
| **Aqua**   | water         | bucket charge, snow, mud traces                                                      |
| **Metal**  | refined ore   | the four ore blocks (bench-processed)                                                |
| **Aether** | the rare one  | Glowvein and Aetherock, high-level bonus lines — **cannot be transmuted into, ever** |

Denominated in the existing sub-block base (`SUBS_PER_BLOCK` sub-units to a block, each of `SUB_UNIT_VOLUME` — both
unchanged).

### 3.1 Extraction table (bench, base yields at full efficiency)

The full-roster table `sub_material()` grows into; the figures live in `alchemy::extract_base`. Base totals are
deliberately **short of a whole block's worth of sub-units** — the shortfall is "structure lost to the process" and is
what makes extract→craft→extract a sink at every level (see §4.2). Within one essence the ordering is the obvious one:
clean rock pays the most, mined rubble a little less, and the loose materials less again.

| block                 | yields                     | notes                                                                                                                                              |
| --------------------- | -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| Stone                 | Earth                      | the pure baseline, a whole block's worth — craft-only in practice (§7.1); closes the recycle loop                                                  |
| **Rubble**            | Earth                      | what mined rock actually is (§7.1); the mass Earth feedstock, just under clean Stone                                                               |
| Sandstone             | Earth                      | craft-only in practice — mined sandstone is Rubble too                                                                                             |
| **Glass**             | Earth                      | crafted blocks extract back at a loss, like everything                                                                                             |
| Gravel                | Earth                      |                                                                                                                                                    |
| Sand                  | Earth                      |                                                                                                                                                    |
| Dirt                  | Earth + a little Flora     |                                                                                                                                                    |
| Grass                 | Earth + Flora              | the canonical split: mostly Earth, a Flora minority                                                                                                |
| DryGrass              | Earth + Flora              | drier, so less Flora than Grass                                                                                                                    |
| Mud                   | Earth + Flora + Aqua       |                                                                                                                                                    |
| Wood                  | Flora                      | the Flora baseline, a whole block's worth                                                                                                          |
| Leaves / PineLeaves   | a token Flora              | foliage is water and structure; the essence is in the trunk (cut from its first draft — against a near-instant dig it paid Flora as fast as grass) |
| Snow                  | Aqua                       | cold biomes' economic identity                                                                                                                     |
| Water (bucket charge) | Aqua                       | unlocked with the Aqua economy (§4.3); consumes the charge. The one direction — the world hands you water, so there is no craft (§7.5)             |
| WoodStairs            | Flora                      | shaped items extract at volume —                                                                                                                   |
| StoneStairs           | Earth                      | crafting is no longer one-way                                                                                                                      |
| WoodDoor              | Flora                      |                                                                                                                                                    |
| WoodSlab / StoneSlab  | (their material's essence) | in their family                                                                                                                                    |
| Emberore              | Metal                      | the tiers rise steeply —                                                                                                                           |
| Duskstone             | Metal                      | each ore is worth                                                                                                                                  |
| Glowvein              | Metal + Aether             | markedly more than the one above it; the first Aether                                                                                              |
| Aetherock             | Metal + Aether             | the richest Aether                                                                                                                                 |

### 3.2 Level scaling

Efficiency climbs linearly with the level, from a fraction of the base for a novice to the whole base at the cap
(`alchemy::scaled_yield` / `extract_yields`, floored per line so a line that pays at all never rounds to nothing). A low
Alchemy therefore leaves most of a block behind, and a rich find hoarded until your Alchemy rises is worth more later —
deliberate.

### 3.3 Bonus lines (the "rarer essence unlocks")

Flat additions, **not** scaled by efficiency — the unlock is the whole point.

**The pump rule (found in implementation, pinned by `bonus_lines_never_sit_on_craftable_blocks`):** a bonus line on a
*craftable* block is an essence pump — craft it from its cost essences, extract it back, pocket the bonus. High enough
up the ladder the originally-drafted lines turned a couple of Earth into Metal (Stone trace minerals) and, worse, a
couple of Flora into Aether (Wood heartwood), violating "Aether cannot be transmuted into, ever". So bonus lines live
ONLY on recipe-less blocks: mined rock (Rubble, Gravel) and foliage. The heartwood flavor moved to pine resin:

The lines in order, each at its own Alchemy threshold (`alchemy::bonus_lines`):

| unlock                                  | flavor                                                |
| --------------------------------------- | ----------------------------------------------------- |
| Leaves and PineLeaves start paying Aqua | "morning dew" — the first line, and the earliest      |
| loose rock starts paying Metal          | "trace minerals" — mined rock, never craftable Stone  |
| that Metal line deepens                 | the mid-ladder rung                                   |
| **PineLeaves** start paying Aether      | "resin" — pine forests are the pre-ore Aether trickle |
| loose rock starts paying Aether too     | the last line, and the deepest                        |

Lines are **cumulative**: each row adds to the ones before it, so an extraction at the top of the ladder yields the
trace minerals AND the Aether, and PineLeaves keeps its dew once it has earned its resin (`bonus_lines` returns every
earned line).

---

## 4. Alchemy (the skill)

### 4.1 XP sources

| action                                           | what the XP is derived from                                                                                                                                                                             |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| bench extraction, per block                      | the block's worth at the bench (`alchemy::extract_worth`) — and **zero for anything the bench itself can make** (`skills::convert_xp`, §7.5)                                                            |
| field ore extraction (the dual-XP cut, see §5.4) | a per-tier figure, rising with the tier (`skills::ore_field_xp`) — and the ONLY Alchemy a vein pays, since raw ore never reaches a bench                                                                |
| transmutation, per craft                         | a fixed fraction of the ideal cost (`skills::craft_xp`), so a slab pays least, a whole block more, and a tool most of all — pinned by `alchemy_xp_derives_from_the_base_tables`, per-line ceil included |

XP keys on the *base* value, never the level-scaled yield — per-action XP is fixed for life, RS-style. Extraction XP is
paid for reducing what the WORLD made and nothing for undoing your own work, which is what stops the extraction and
transmutation rows composing into a converter from essence into levels; the bench's PACE follows a block's worth either
way, so a planed timber comes apart no faster than the log it was cut from.

**Both halves of the bench arrive dripped.** The extract tab sends one `Convert { count: 1 }` per completed cycle and
the transmute tab one `Craft { count: 1 }` (§8.1), so a hoard of stone and a hundred-slab batch are each a long steady
pull of Alch XP rather than a click — feeding the coalescing XP toast a steady pulse. Crafting was the exception until
it wasn't: it answered instantly, which made the bench's one immediate action also its fastest XP by a wide margin, and
one CTRL press paid a hundred crafts in a frame. The award did not change and should not — transmuting is the skill's
main verb — but it now takes the time the work takes, off the same `base_pace_ticks` curve scaled by
`alchemy::craft_worth`. The catalog's biggest price is therefore also its longest hold, which is the reading a forge
should have. `ALCHEMY_MAX_COUNT` stays in the protocol untouched: the multipliers still choose a batch size, and the
wire doesn't care who paces it.

### 4.2 Transmutation cost

Every line of a recipe is charged its ideal times a **waste multiplier that falls with the level**
(`alchemy::waste_permille` / `craft_cost`): steep for a novice, easing toward parity at the cap, and monotonically
non-increasing throughout, with a late unlock clamping the tail early. Waste never reaches parity — a craft always costs
more than the ideal.

`ideal` stays the shipped volume rule — `raster_volume(...).div_ceil(SUB_UNIT_VOLUME)` — so the pinned
`craft_costs_derive_from_collision_volume` test keeps working with a `waste` multiplier layered on top. Raw-block
recipes cost the **per-line ceil of a whole block's volume split** in the block's extraction ratio — each line rounds up
separately, so a multi-essence block's ideal lands a little above a single-essence one's — which combined with §3.1's
deliberately short extraction bases yields the no-matter-creation pin with margin to spare.

### 4.3 The Alchemy ladder

The order below is the design; the thresholds themselves live in `unlock.rs` (`shape`, `material`, `functional`,
`extract`) and the prices in `alchemy.rs`. Read it as a sequence, not a schedule — what matters is what comes before
what, and why.

**The opening** hands you the basic extraction roster and the **Slab** shape at once, with **Stairs** a few levels
behind it — the two forms every builder wants first. The **Torch** (§7.3) comes next, deliberately *before* any ore
exists: light is what makes a cave enterable, so it must not wait on the thing you go into caves for. Then the **Essence
Siphon**, timed to land alongside Mining's first ore reveal — the first cross-skill handshake, and the reason M4's tool
machinery had to ship ahead of M5. (The **Door** shape was authored here; it ships with the planed-timber set instead —
see below.)

**The Aqua economy opens** with extraction of the bucket charge, and the **planed timber** material arrives with its
whole joinery set — Lumber block, slab, stairs and door together, because §7.2's `max(shape, material)` rule lets the
material dominate every shape it is worked into. The first real material wave follows: **clean Stone** (the mason's
unlock, §7.1) — which arrives alone, since the loose ground and the felled log beside it in the ladder are things the
world hands you and §7.5 keeps them off the bench; then **Stone Bricks**; then Sandstone and **Glass**. Interleaved with
them are the first bonus line (dew, §3.3) and the first quickening of the extraction pace, which every block's own pace
scales with.

**The middle** is shapes and sinks: **Fence** and **Ladder**; the trace-minerals bonus line with the **Lantern** and
**Framed Window** to spend Metal on; **Pillar** and **Roof Wedge** alongside **Polished Stone** and **Sandstone
Bricks**; then **Metal**, the prestige material, which brings its whole grid row (MetalStairs, MetalDoor, …) with it.
The **Resonant Pick** is the next rung, priced to be the game's biggest single essence sink.

**The endgame** adds **Pane** and **Gate** (Glass Pane, Metal Bars) with the lossy essence exchange (any → any, **except
into Aether**), then **Trapdoor**; the pine-resin bonus line and the last quickening of the pace; the **Aetherlamp**,
the one repeatable Aether sink; a clamp that brings the waste multiplier to its floor a little early; the last bonus
line (Aether from loose rock — never from craftable Stone, which would be §3.3's essence pump); and at the cap, full
extraction efficiency, the floor of the waste curve, and the **Master's Sigil**.

Material and shape unlocks compose per §7.2's `max(shape, material)` rule — a StoneDoor lands with the Stone material,
not at a curated late number of its own.

---

## 5. Mining (the skill)

### 5.1 Breaking strength (SIM v18: per-material families + the strength wall)

**`hardness_ticks` is a base the curve scales, not a speed at any reference level — per FAMILY.** Every diggable block
carries a `DigFamily` (`blocks!`'s `dig` builder takes it beside hardness, so a row can't forget it), and a family
declares only TWO numbers: where it starts at the first level and where it ends at the cap. The SHAPE of the journey
between them is authored ONCE, in a shared taper table, so no family can drift from the others:

| family    | members                                         | why                                                                                                                         |
| --------- | ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| **Fixed** | Leaves, PineLeaves, Torch, Water (scoop)        | already near-instant / fixtures; the scoop is pinned to the viewmodel swing, so its endpoints are equal and it never scales |
| **Soft**  | Dirt, Grass, DryGrass, Sand, Mud, Snow          | the gentlest climb — the shallowest penalty for a novice                                                                    |
| **Wood**  | Wood + its shapes                               | timber resists a little more than soil, and masters a little less                                                           |
| **Rock**  | Stone, Sandstone, Gravel, Rubble + stone shapes | rock resists a novice viscerally — the steepest start, and it ends level with Soft                                          |

`skills::dig_speed_permille` walks the family's two endpoints along `DIG_TAPER`, and `dig_required(block, h, L)` scales
the block's `hardness_ticks` by the result. `DIG_TAPER` is how much of that journey is complete at each step, in
permille — a pinned integer table, never a `powf`, for the same reason the XP curve is one: it is a golden both ends
must agree on to the unit, and a float would let two platforms round a hold apart. The endpoint at the cap IS the speed
floor (no second clamp knob), and the flat Fixed family gets its flatness for free from a zero-width range, which is
what keeps leaves/torches/the scoop from ever speeding up.

Everything below argues about what a dig *feels* like, so the absolute scale it argues from: Stone runs 3.83 s at level
1, 1.90 s by 30 and 0.63 s at the cap; Wood 2.03 s → 1.10 s → 0.50 s; Dirt 0.93 s → 0.50 s → 0.22 s. Those durations are
the subject of every claim in this section — the code is still the authority on what the curve returns.

**The curve is sampled every fifth level, plus the cap — and every jump is smaller than the one before it.** Those are
one decision. `skills::dig_speed_step` holds the multiplier flat between steps, so a digger speeds up only on the
sampled levels and once more at the cap — a handful of felt jumps in a life, where a per-level curve would have one at
every level and none of them noticeable. Under a per-level curve most levels moved a hold by a single tick or by nothing
at all, and a reward you cannot feel is not a reward; concentrating the same total speedup into a fraction of the levels
is what makes it legible. The taper then spends that budget front-loaded — each increment lands about an eighth under
its predecessor — so **the first step is the biggest speedup a player will ever feel**, and every later one lands a
little softer. Both halves earn their keep: the early game is where a slow dig is most discouraging, and diminishing
returns are what stop the last rungs from being rounding errors. The relative gain falls too, not just the absolute one
(on stone, 11.7% off the hold at the first step against 2.6% at the last), so the easing is felt and not merely counted.
Only the two endpoints are anchors and both must sit on the sampling grid, which is why the cap is a step of its own
instead of the grid ending a rung short. `dig_speed_step` is the ONE place the step size lives, and
`dig_speed_step(l) == l` is exactly the question "does this level make me faster" — which is what the UI has to ask to
tell a player when the next jump lands.

**No level is parity with the hardness table any more (SIM v18).** From v11 through v17 one mid-ladder level sat at
exactly 1× for every family, which made that level reproduce the shipped hardness table exactly, and the tests pinned
`dig_required` against it for every block. v18 rebuilt the curve around its two endpoints and lengthened the early game
— markedly slower at level 1, the cap untouched — so every level is now an ordinary sample of the taper and that
assertion is gone with it. The consequence worth remembering: **no test may hardcode a hold length**; derive it from
`skills::dig_required` (two server dig tests carried a literal one and broke on the retune).

**Letting go decays, it does not reset** (`skills::dig_decayed`). A released hold keeps its banked ticks and bleeds them
back to untouched at **one whole bar per `DIG_DECAY_TICKS`** — so that window is the *ceiling*, paid only by a block a
tick from breaking, and a barely-started hold is gone almost at once. A hold resumed inside the window inherits what is
left, and the crack reads the same number, so it visibly retreats and picks back up rather than snapping between full
and gone. Setting the rate as a fraction of the bar rather than in ticks is what makes the web retreat at one visible
speed on leaves and on stone alike.

Both ends run that one function over their own release→resume interval. On a stable link those intervals are the SAME
length — the client's release and its restart are each shifted by the same one-way delay — so only the jitter between
the two separates them, which is what `dig_grace` already absorbs. The server also errs the safe way at every seam:
`DIG_OFF_TARGET_RESET` starts its decay a beat *after* the client gave the cell up, and `verify_digs` runs ahead of the
action queue, so its banked number is never below the one the player's bar is showing. Progress is stored as the banked
PEAK plus an idle count, never decremented in place, so the two sides compute one number from the same two integers
instead of two rounding chains.

**The strength wall** (`unlock::mine`) is a deliberate reversal of this section's original "no strength wall" stance,
and it is keyed on the **DIG FAMILY** — the same axis that decides how long a hold takes, which is the only thing raw
strength can honestly be about. Fixed and Soft from the first swing, then **Wood**, then **Rock**: soil → trees → stone,
each rung opened by digging the one below it — a modest run of dirt digs buys the timber rung, and a shorter run of logs
buys the quarry, since a log pays more XP than a clod. Every block of a family opens together, shapes included, which is
what stopped rock being the only family with a wall at all. Gravel is no longer the ungated "entry rock" — timber is
what now stands between bare ground and the quarry. A handful of substances must be EARNED on top of their family's wall
(`mine_material`: the four ores at their own reveal, Metal and Crystal at their own later levels) and a block waits for
the later of the two — the material table can only ever push a block later, never earlier. Refusal happens at `DigStart`
via `RejectReason::SkillGated` (protocol v17) — the `Protected` pattern, so nobody holds a button through a dig timer
for nothing — and the client predicts it in `diggable()` (it knows its own level; `unlock::Kind::Strength`'s message is
the shared wording). Ore (§5.3) stays gated by *visibility* on top of this.

**The craft gate** (`unlock::pickup`, SIM v13 / protocol v19) is the same refusal on the other skill: a **craft-only**
block — one worldgen never places, marked `crafted` in its `blocks!` row and pinned against worldgen itself by
`worldgen_never_places_a_crafted_block` — refuses to be dug below the Alchemy level of its own §4 recipe. Every one
standing in a world was made by somebody, so picking one up is a way of *having* it, and mining must not route around an
unlock: WoodStairs/StoneStairs wait for the Stairs shape, the Torch for its own hand-set level, the whole planed-timber
set for its material (slabs sit at the bottom of the shape axis, i.e. ungated in practice). The gate keys on the DROP,
not the cell, which is what keeps pristine Stone a Mining question — it hands out Rubble, and Rubble has no recipe at
all — and on craft-*only* rather than craftable, or `Wood`'s own recipe level would shut a fresh player out of the first
tree they see. Same machinery as the strength wall: refused at `DigStart` with `RejectReason::NotUnlocked`, predicted in
`diggable()`, one shared wording in `unlock::Kind::Recipe`'s message ("you haven't unlocked this yet"). Placing, opening
and walking on other people's builds are all untouched — only the pickup is refused.

The server's completion gate already carries `dig_grace(need)`, a few ticks of slack that widens with the hold; a
level-up landing mid-hold can skew client/server `required` by a tick or two and the grace absorbs it — no new tolerance
machinery. Bedrock at the world floor stays undiggable, as ever.

### 5.2 XP per dig

A fixed fraction of the block's `hardness_ticks` (`skills::dig_xp`), awarded in the existing dig-credit block — so the
pay order is the hardness order: leaves least, then snow, then the soils, then gravel, wood, sandstone and stone. Shaped
blocks pay by their own ticks, so a slab third pays a third's worth. Ore field-extraction pays a per-tier Mining figure
instead, rising steeply with the tier (+ the Alchemy cut, §5.4).

**Then the depth reward scales it** (`skills::depth_xp_permille`): flat at and above `DEPTH_XP_FROM` — the waterline, so
nothing about surface mining moves — then rising linearly with depth to `DEPTH_XP_MAX_PERMILLE` at `DEPTH_XP_TO`. Both
ends are chosen rather than convenient. The far end is the deepest cell a dig can happen in **at all**, one above
bedrock, because bedrock is undiggable and a maximum nobody can ever be handed is not a maximum. The ceiling is bounded
from one side only: a larger one stops being a reason to go down and becomes a tax on staying up. Nothing bounds it from
underneath any more, because `dig_award` rounds to NEAREST rather than truncating — so any ramp above unity moves every
award in the game, including the blocks at the very floor of the scale, which pay a single point flat and which a
truncating ramp could not have moved by any multiplier under two (`the_depth_reward_moves_every_award`). That the
guarantee is structural rather than lucky is what lets the award scale be retuned without quietly stranding the bottom
of the hardness order on a flat number. Half again is a pull a surface player can still decline.

**It is linear per block, deliberately NOT quantized into depth layers** — the opposite of what `DIG_STEPS` does to the
dig curve, and for the same reason that grid exists. The dig curve had to be quantized because a per-level change moved
a hold by a single tick or by nothing, so the reward read as noise and had to be concentrated to be felt at all. XP has
no such problem: the awards are small integers, so the rounding quantizes them for free and every block type gets its
own ladder of whole-point steps — stone climbs from its flat award to its deepest one point at a time, evenly spread
down the range, with no value in between skipped (`the_depth_reward_climbs_one_point_at_a_time`). Layering on top of
that would add nothing a player can see, and it would buy cliffs: a handful of depths where one more block down is worth
a permanent step and every block after it is worth nothing. A straight ramp is monotone, with no threshold worth
standing on.

**Extraction is exempt, and loses nothing by it.** The ore tiers are depth-banded already — Aetherock is worth what it
is worth *because* it sits just off bedrock — so scaling `ore_field_xp` would count the same axis twice and turn §5.3's
authored figures back into a consequence. And an extraction excavates nothing: it leaves a DepletedStone face, and
clearing that face is an ordinary dig that scales like any other. The miner at a deep vein is already paid for depth,
for the rock they actually broke.

**A wide swing is a bonus in reach, not in XP** (§5.6). The cell under the crosshair pays its whole award; every other
cell the Resonant Pick's plane took pays `PICK_EXTRA_PERMILLE` of one — floored at a single point, because a fraction of
the cheap end of the hardness order is nothing at all, and capped at the award itself, so that floor can never out-pay
the cell it is a fraction of. The pick already pays for itself in **material**: one hold hands back a whole plane of
blocks, and the hold is only a level-only multiple of a single-cell one rather than the plane's worth of time. Paying
full XP per cell on top of that would make the tool the only sensible way to earn Mining at all, and would turn each of
its two reach milestones into a step change in levelling speed rather than in what the tool *does*. The fraction stays
in permille until the whole swing rounds once (`a_wide_swing_pays_the_aimed_cell_in_full_and_the_rest_a_fraction`) — a
per-cell rounding would drift further from the fraction the further the pick reaches, which is exactly backwards.

`skills::dig_award` is the one call that puts all of it together — the flat table, the swing count, the depth reward and
§5.4's split — so neither the server's chokepoint nor the engine's offline arm can read the tables and forget the rest.
It keys on the **anchor** cell: a wide pick swing against a vertical face spans several rows, and the anchor is the one
cell both ends provably agree on, since the server validates the request against its own ray. The whole swing is scaled
once rather than cell by cell — it is one action, and one rounding. The character screen carries the live figure under
Mining, because a reward a player cannot see coming is exactly what §5.1's step grid already exists to fix.

### 5.3 Ore

Four ores + a depletion block = **five new `blocks!` rows** (append-only, one PNG + `LAYERS` entry each; names are
placeholders until they survive contact with you):

| block             | reveal                                               | depth band                                      | bench yield                                         |
| ----------------- | ---------------------------------------------------- | ----------------------------------------------- | --------------------------------------------------- |
| **Emberore**      | the first, mid-way up the Mining ladder              | the shallowest — surface caves already reach it | Metal                                               |
| **Duskstone**     | the second                                           | deeper                                          | Metal                                               |
| **Glowvein**      | the third, with the Resonant Pick                    | deeper still                                    | Metal + the first Aether                            |
| **Aetherock**     | the last, near the cap                               | the deepest, just off bedrock                   | Metal + the richest Aether                          |
| **DepletedStone** | — (never revealed; it is what a tapped cell becomes) | —                                               | `drops: Some(Rubble)` — the tapped-rock face (§7.1) |

Each tier is harder, richer and rarer than the one above it, and the bands overlap (see the divergence note at the top),
so a tier a player has earned stays available all the way down. Reveal levels are `unlock::reveal`, hardness and yields
their `blocks!` and `material.rs` rows; ids come from the append-only `blocks!` table, and the ones this section
originally reserved were taken by later rows.

**Glowvein carries the first Aether, and that is a fix rather than a flourish.** As originally built, every Aether in
the game sat past the pick: the deepest tier reveals near the cap, the pine-resin bonus line lands high on the Alchemy
ladder, and loose rock's Aether rung is the last one there is. Crystal is the one other row that yields Aether and it is
a **sink** — craft-only, and §4.2 guarantees making one costs more than taking it apart returns, so it can never be a
net source. The Resonant Pick's price names Aether and unlocks at neither of those levels, so for a long stretch of the
ladder the pick was unlocked, advertised on the character screen, and impossible to build. Putting a reliable Aether
line on the tier that reveals at the pick's own level closes it without moving a threshold: the supply arrives with the
demand rather than a ladder later, and §7.3's price is left saying exactly what it always said.

`every_functional_price_has_a_source_at_its_own_level` is the pin that would have caught it. `functional_ideal` and
`unlock::functional` were already pinned as twins, so an item could not have a price without a level — but nothing asked
whether the price was *payable* at that level. The new pin walks every functional row and, for each essence its recipe
names, demands some block a player at that level can already dig and take apart. What counts as a source is narrow on
purpose: the world has to place the block (a craft-only substance is a sink by §4.2), both walls apply, and only what a
tier pays **reliably** counts — a price payable when the world happens to be generous is not payable.

**The Duskstone trace is the luck beside that ladder.** A share of the middle tier's cells carry a unit of Aether, so a
player who has been working that tier all along may walk up to the bench already holding what the pick wants. It is a
rate rather than a yield line because the finest an essence split can be cut is a whole unit, and a unit per draw is far
more than a middle-tier seam should be worth — the only way to price it below that is as a chance of one
(`ore::traces_aether`). Derived from the cell and the world seed exactly as the deposit itself is, never rolled: the
server credits it, the offline arm credits it, and a rejoin years later agrees, with nothing stored and nothing on the
wire. It is deliberately excluded from the pin above, because a supply nobody can plan around is not one a price may
depend on.

**Placement** — `ore_at(x, y, z, seed) → Option<OreKind>`, pure, in `worldgen/`: per-family salted position hashes seed
deposit centers (the `structures_near` pattern, so pockets span chunk borders statelessly); a pocket is the handful of
cells within its blob mask. An ore manifests only where the **pristine** cell is Stone **and the cell is unedited**
(client: the `EditOverlay`/pipeline knows; server: the compacted journal's per-cell LWW map answers "has this cell ever
been written"). The unedited rule is the anti-dupe: placing stone into a deposit coordinate can never regenerate the
vein. Deeper = richer tiers, so the level gate and the dig-deeper incentive reinforce each other.

**Reveal** — the mesher stamps ore texture layers over qualifying Stone cells when `mining_level ≥ reveal`; the pipeline
injects the level into the padded mesh snapshot the same way `EditOverlay` already intervenes. Crossing a reveal
threshold bumps generation counters on resident chunks — a remesh sweep, sliced through the existing stream budget.
Veins *fade in around you* on the level-up. Below the gate the client renders, raycasts, and digs plain Stone, and the
server agrees entirely — a low-level player digging a would-be vein cell gets stone and spends the vein. You dig what
you can see.

**Extraction at the rock face** — requires the **Essence Siphon** (§7.3) as the active tool. With it held, a dig-hold on
a revealed ore cell is an extraction: on completion both sides derive "this is an ore extraction" from `ore_at` + the
digger's level + the active tool (the `Layout` tool entry the server already holds, in wire order like every slot check)
and write **DepletedStone** instead of Air, crediting the raw ore item. `EditRequest{kind: Dig}` unchanged — no new
wire. Without the siphon — bare hand or pick alike — a revealed ore cell **refuses the dig at `DigStart`** via the
existing `ServerMsg::DigRefused` + `refused_cell` machinery, with its own message: if you can see the vein, the game
won't let you stone-dig it away by accident. Below the reveal gate nothing changes — stone before, stone after, vein
spent — because that player's client and server agree there is nothing there to protect.

Consequences, all free: mining ore never excavates (the mountainside keeps its shape); below-gate players see stone
before and after; the depleted face gives the miner visible feedback; and the write being a *different id* means the
edit is unambiguously real to the journal, LWW replay, and mesh invalidation (no same-value-write no-op risk).

**The draw** — extraction shows no crack decal (`dig_progress` reports `None`, the water-tool precedent); the feedback
is the pull. Through the hold, the ore's emissive seams (§7.4's Stage-A bit, driven by a progress value instead of a
constant) brighten from their resting glitter to full bloom, while a stream of motes in the ore's color detaches from
the face and arcs into the held siphon — a small world-space emitter in the `spray.rs` mold, so remote players watching
a miner work see the stream too. Completion pops a brief burst and leaves the DepletedStone face; a canceled hold lets
the seams decay back. The siphon's viewmodel is the other half of the effect — the draw-in point the motes converge on.
Raw ore is an ordinary slottable, placeable item — a *placed* ore block is genuine chunk data visible to everyone at any
level and diggable at face value (dupe-safe round trip); showcase your finds.

**What shipped instead, and why the paragraph above is kept.** The two sentences before this one describe the extraction
as it was built and NOT as it was meant to work: a siphon was always supposed to draw the essence out at the rock face,
which is the whole reason the tool is an *Essence* Siphon and the whole reason a field draw pays Alchemy XP at all. The
implementation credited the raw ore block instead, which made ore a thing you carried to a bench that already had a
better way to get there — and made "does a placed ore block glow?" a question with no good answer once veins started
casting light (GI-DESIGN R31).

Three corrections, and every one of them makes the section above simpler rather than adding to it. A draw credits
**essence** (`alchemy::field_yield`: the ore's own bench yield, so the Alchemy level scales a field draw exactly as it
scales the bench, and Mining's capstone multiplies the whole split). A tapped cell becomes the **tier's own spent rock**
rather than one generic grey face — same rock, same flecks, the light gone out of them — which is what makes a worked
seam still read as the seam you worked. And **that** rock is the trophy: it drops itself, it is ordinary chunk data with
no reveal gate, so a visitor at Mining 1 sees your wall of tapped Aetherock exactly as you do. Raw ore is therefore
never an item at all, and `is_slottable` says so, which is both the rule and the migration for saves written under the
old behaviour.

### 5.4 Dual XP

Field extraction is mechanically a mine, thematically a crude alchemical separation: full Mining XP plus an Alchemy cut
at the rock face (`skills::ore_field_xp`), so a miner who never touches the bench still moves the other skill.

**The cut at the face is the whole of it.** Raw ore is never an item (§5.3), so no ore reaches an extraction slot and
there is no bench moment behind the field one — which is why the Alchemy column is not the token it would be if it were
splitting a total with a later payment. `alchemy::extract_total_base` still prices a tier and `convert_xp` still answers
for it; that answer is unreachable, and is kept as the basis the column is set against. Alchemy's bulk is still the
bench, taking apart what the world hands you (§4.1) — a supply no seam competes with.

**The tiers do not follow a retune of `dig_xp`**, and that is the point of tabling them. A vein is scarce and finding
one is most of the work, so a draw is priced against its tier rather than as a multiple of the rock it sits in; the
hardness-derived award and the ore table are two separate decisions, and moving one is a deliberate lever on how much a
seam is worth *relative* to quarrying rather than a figure drifting out of step.

### 5.5 The Mining ladder

In order; the thresholds live in `unlock.rs` (`mine`, `mine_material`, `reveal`, `pick_cells`, `prospector`,
`ore_mastery`) and the curve in `skills.rs`.

**At the first level** every family digs at its own multiple of the hardness base (§5.1 — the climb back down those
multiples *is* the early game), and only soil and foliage are diggable at all. **Timber opens** next — the Wood family
entire, grown and planed — and then **rock**: gravel, sandstone, stone, rubble and their shapes, all at once (§5.1's
strength wall). Every fifth level after that steps the family curve one rung, never in between, each step smaller than
the last.

**The ore reveals** are the spine of the rest: **Emberore** first, mid-ladder, its extraction needing the Essence Siphon
(§5.3) — the first cross-skill handshake; then **Duskstone**; then **Glowvein**, arriving with the level that makes the
**Resonant Pick** usable; and finally **Aetherock**, near the top. Past the last reveal the ladder pays in refinements
rather than new substances: the pick's resonance deepens by a cell (§5.6), **prospector's sense** adds a faint audio
shimmer within `unlock::prospect_range` of an unrevealed-tier deposit (a cadence emitter, like footsteps — audio only,
no ESP overlay), and mastery at the cap adds a bonus ore to every field extraction.

### 5.6 The Resonant Pick

Item-only `blocks!` row (like all tools, §6). Crafted at an Alchemy level and usable at the matching Mining level — the
flagship cross-skill dependency. Held, a dig-hold on any diggable cell also digs the cell **directly behind it along
−normal, if it holds the same block id and would credit the same item** (§7.1 — provenance decides a drop, so id alone
stopped being enough); `required` carries a level-only multiplier over the single-cell time
(`skills::dig_required_with`); both drops land in full, and the XP is the aimed cell's whole award plus a fraction of
one for everything behind it (§5.2). The pair is derived from the server's own re-raycast (cell + face normal), never
client-chosen — no cheat surface. Writes ride `begin_multi` under one seq exactly like door pairs. At the later Mining
threshold the reach extends by another cell (`unlock::pick_cells`). Protection checks run per-cell via the assembled
`writes` list, as they already do.

---

## 6. Tools and switching

The current pinned-slot WaterTool is not dismantled — it is **formalized**. The hotbar becomes its block slots plus a
dedicated **tool slot**, drawn visually separated from the block row, with the last digit / scroll reaching it exactly
as today. Which tool sits *in* the slot is a separate axis:

```
[1][2][3][4][5][6][7][8]   ┃ [9]┃ ▫ ▫
 blocks (digits 1–8)         tool   faded rail: other owned tools
```

- **Ownership is inventory counts.** Tools are item-only `blocks!` rows — `RenderClass::None`, `collides: false`, not
  placeable, and **`is_slottable: false`** like water. Crafting the pick is `inventory.add(pick, 1)`; the tool rail
  lists every nonzero tool id; `reconcile_layout` keeps tools out of the block slots and the grid with the rule it
  already has. Ownership, prediction, rebase, and both persistence formats all ride the existing count machinery
  untouched. Launch roster: the **bucket** (owned from the start, as today), the **Essence Siphon**, the **Resonant
  Pick** — three tools, which is exactly when tap-9 cycling and the rail earn their keep. Shipped: **four**, since
  §7.3's **Master's Sigil** is an item-only row you hold and so is `is_tool` too — it does nothing, but it caps at one,
  cycles and draws in the rail like the rest, which is exactly the point of routing ownership through the counts.
- **The two crafted work tools carry their own light** (built after the fact; GI-DESIGN.md R30). The Essence Siphon and
  the Resonant Pick each `emit` well below a torch, in the colour of the thing they deal in — the siphon's drawn essence
  green, the pick's resonance blue — so the work they exist for, which happens at the bottom of a mine, does not have to
  be done one-handed with a torch in the other hand. It costs the catalog nothing: a carried light is an ordinary
  emitter, and the tint comes from the row's own texture. **The two are not equals, and that is the progression**: the
  siphon throws a pool at the feet — it extracts for a living and glows incidentally — while the pick **steps up at its
  second rank** into the best light either hand holds short of a torch, keyed on `unlock::pick_cells` rather than a
  threshold of its own, so the level that widens the swing is the level the tool visibly answers to. The step whitens
  the light as well as raising it, because a saturated blue carries so little luminance that level alone reads as more
  blue rather than as brighter (GI-DESIGN.md R30). The tool itself stays blue; what it throws goes pale.
- **Cycling**: tapping **9 again while the tool slot is already selected** advances the active tool through the owned
  set, wrapping. No new keys, and touch gets the identical rule for free — first tap of the tool slot selects it,
  further taps cycle. Selecting the slot by scroll then tapping 9 also cycles (the rule is "9 on an already-active tool
  slot", nothing subtler). With one tool owned the extra taps no-op; the rail only draws when a second tool exists.
  Backward cycling is deliberately omitted — the roster is small and it wraps; Shift+9 can grow that meaning later
  without disturbing anything.
- **The faded rail**: small icons at reduced alpha beside the tool slot, active tool excluded, in a fixed owned-order
  (block-id order — deterministic, no arrangement state to persist or sync). It's a readout, not a target: clicking is
  not required, cycling is the interaction. Built as faded *slots* rather than bare icons — they sit over open world,
  and a dark tool on dark terrain was a smudge.
- **The swap is a handover, not a cut** (built after the fact, and it is what makes cycling read as one row rearranging
  itself rather than an icon glitching): the outgoing tool shrinks away and the incoming one grows in, through the same
  curve the transmute tab's family tiles rotate on (`ui::handover`). Everything else about the exchange — which tools
  the rail lists — happens at the midpoint, where every icon is at nothing, so no tool is ever in the slot and the rail
  at once. `ToolSwap` watches the active tool rather than being told about the keypress, so a server `Layout` rebase and
  a rejoin animate the same way a tap does.
- **Wire**: `Layout.hotbar` keeps its entry count and its format; the last entry's meaning becomes "the active tool id"
  (the sentinel `WIRE_TOOL` decodes as the migrated bucket id). Slot enforcement is unchanged — `EditRequest.slot`
  naming the tool slot + the server's copy of the active tool validates Place/Collect exactly as `WrongSlot` does today.
  Cycling is a `Layout` send, which already precedes edits on the reliable stream, so a cycle-then-scoop in one tick
  validates in wire order. `HELD_TOOL_FULL` stays as the remote-display sentinel for the charged bucket.
- Tool *use* gates (the pick's own Mining level) are checked server-side at dig validation and mirrored client-side in
  `diggable` — an unusable owned tool still cycles into the slot but digs like a bare hand, so there's no hidden-tool
  edge case in the rail.

---

## 7. Expanded transmutation catalog

### 7.1 Rubble — why transmutation *is* the economy

Digging natural rock never yields the clean block. Stone's `blocks!` row carries `crumbles: Some(Rubble)`, so every
mined stone lands in the inventory as **Rubble** (new id, append-only): a cracked, ugly, fully placeable block. Clean
Stone exists only two ways: pristine in the terrain, or transmuted at the bench. The loop this creates is the point — a
builder who wants clean walls *needs* Alchemy, not just a miner's patience:

```
mine Stone → Rubble  →  extract (Earth)  →  craft clean Stone (whole-block ideal × waste)
```

- **Rubble is not junk**, and since §7.5 it is not optional either: it is the *only* thing a natural quarry hands you,
  because clean rock is now strictly bench work. Placed, it reads as rough cobble/drystone — the free rustic building
  material. Low-level builds are cobble builds; that's aesthetic progression. Its texture is authored to carry that
  weight (`tools/texgen`'s `rubble`): a wrapping Worley partition cuts the tile into cobbles at visibly different tones,
  because the two rock faces are a decision a player reads off a wall and the earlier recipe — stone's own body with
  fracture walks over it — could not be told from smooth stone at any distance.
- **Sandstone crumbles to Rubble too** ("all broken rock"), so clean Sandstone is bench work in exactly the same way.
  Sand, Gravel, Dirt are already loose material and hand themselves back.
- **DepletedStone breaks into Rubble** — a tapped-out vein face crumbles like any other rock. It uses the unconditional
  `drops` and not `crumbles`: every cell that names it was written by an old siphon draw, so the conditional form would
  start handing out a legacy block that has never been obtainable.
- **No hybrid recipes.** Refining is extract-then-craft through the existing `AlchemyOp::Convert`/`Craft` shapes — there
  is no "1 Rubble + N Earth → Stone" op, which would be new machinery for no gain. The Rubble→Stone exchange rate falls
  out of efficiency × waste and nothing else: several rubble per clean stone for a novice, barely more than one at the
  cap — the climb down that ratio *is* the mason's progression.
- Crafted/pristine clean Stone still extracts at a full block's base (§3.1), so the recycle loop closes and stays a
  sink: even at the cap the craft costs more than the extraction hands back.

### 7.2 The material × shape grid

Per-item curation doesn't scale to "metal stairs, stone doors, brick slabs, …". Two unlock axes replace it for
structural pieces:

**Materials** — can you transmute this substance into block form at all. In ladder order (levels in `unlock::material`,
costs derived in `material.rs` from a whole block's essence split):

| material                                                             | notes                                                                                                                     |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| **Planed Timber** (`Lumber`)                                         | shipped; the joinery material, so its slab/stairs/door all land with the material rather than at their shapes' own levels |
| **Stone** (the mason's unlock, §7.1), Wood, Dirt, Sand               | the first real wave                                                                                                       |
| **Stone Bricks**                                                     | same cost as Stone, squared-off dressed texture                                                                           |
| Sandstone, Snow, **Glass** (sand-flavored Earth), **Grass**, **Mud** | the multi-essence substances arrive here                                                                                  |
| **Polished Stone**, **Sandstone Bricks**                             | the dressed pair                                                                                                          |
| **Metal**                                                            | a whole block's worth of Metal essence; scarce by construction, the prestige block                                        |

**Shapes** — can you work any substance into this form. Also in ladder order (`unlock::shape`; each shape's ideal is its
own volume under the rule §4.2 already uses, so no shape needs a hand-set price):

| shape                             | notes                                             |
| --------------------------------- | ------------------------------------------------- |
| Slab                              | the first form, and the cheapest                  |
| Stairs                            |                                                   |
| Door                              |                                                   |
| Fence (posts + two rails), Ladder | Ladder is climbable = §12                         |
| Pillar, Roof Wedge                | the wedge is a half-prism and reuses ramp physics |
| Pane, Gate                        | a gate is a fence-height door                     |
| Trapdoor                          | a horizontal door, so a new facing axis           |

A shaped item in material M gates at **`max(shape level, material level)`** — so a StoneDoor waits for Stone rather than
for a curated late number of its own, MetalStairs and Metal Bars wait for Metal, and a Brick Slab arrives with the
bricks. Each authored pair is still its own `blocks!` row + PNG + baked template, append-only — the grid is the *gating
rule*, not a promise that every cell exists (nobody is owed Snow stairs). Costs stay volume-derived, so the pinned
volume test covers every pair for free, and multi-essence materials (Grass, Mud) split shaped costs in the same
extraction ratio §4.2 already defines for raw blocks.

### 7.3 Functional items (authored costs)

Matter is priced by volume; **function is priced by hand** — the Resonant Pick set the precedent (a pick's cost is
obviously not its volume). Multi-essence costs are deliberate here: cross-family recipes make the essences feel like an
economy, not parallel currencies.

In ladder order; the prices live in `alchemy::functional_ideal` and the levels in `unlock::functional`, which are pinned
twins so an item can't have a level without a price.

| item               | priced in                              | notes                                                                                                                                                     |
| ------------------ | -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Torch**          | Flora + Earth                          | first light, pre-ore on purpose; emissive (§7.4)                                                                                                          |
| **Essence Siphon** | Flora + Earth                          | the ore-extraction tool (§5.3); **no Metal in the cost, ever** — ore needs the siphon and Metal needs ore, so a Metal-priced siphon deadlocks the economy |
| **Framed Window**  | Earth + Flora                          | glass-in-wood-frame, the early window                                                                                                                     |
| **Lantern**        | Metal + Earth                          | metal-and-glass; first Metal sink outside tools                                                                                                           |
| **Resonant Pick**  | Metal + Earth + Aether                 | tool (§5.6); the catalog's biggest single price, and the row whose Aether line put Glowvein's there (§5.3)                                                |
| essence exchange   | any → any **except Aether**, at a loss |                                                                                                                                                           |
| **Aetherlamp**     | Metal + Aether                         | brightest light; the standing Aether sink                                                                                                                 |
| **Master's Sigil** | Aether alone                           | cosmetic held item, and the catalog's last line                                                                                                           |

The Aetherlamp matters economically: before it, Aether's only sinks are the pick (12, once) and the Sigil (99, once) —
two purchases in a whole game. A repeatable 3-per-lamp sink is what keeps Aetherock worth mining at the top of the
ladder, and the contrast between those figures is the entire argument for the lamp existing.

**A price has to be payable at its own level**, and a hand-priced table cannot be trusted to notice when it is not: the
pick's Aether line was unpayable for a long stretch of the ladder and nothing caught it, because the twin pin between
this table and `unlock::functional` only ever asked whether an item had *both* a price and a level.
`every_functional_price_has_a_source_at_its_own_level` asks the other question — for each essence a row names, is there
a block a player at that level can already dig and take apart that yields it — and is what stops a new row repeating it.
See §5.3 for what the pin counts as a source and why the count is narrow.

**Tools cap at one** — `inventory::cap()` returns 1 for every tool id, the exact water-charge precedent. A second craft
refuses before any debit (the standing `InventoryFull` rebase answer), and the transmute row for an owned tool draws dim
with **OWNED** where its button was — a trophy line, not a vanished entry, reusing the locked-row pattern (§8.1). Same
rule covers the Sigil. The alternative — granting the pick automatically at a Mining level — was considered and
rejected: it deletes the craft-it-in-one-skill/use-it-in-the-other dependency and the game's biggest essence sink (240
Metal + 60 Earth + 12 Aether, more than any other single line in the catalog — which is what makes it a sink at all),
and it breaks "ownership is inventory counts" by needing a new grant-on-level-up path with its own rejoin and migration
questions.

### 7.4 Light sources and the lighting question

The engine has sun + shadow cascades and no point lights, so light ships in two honest stages:

- **Stage A (ships with the catalog): emissive materials.** A flag on texture layers (`LAYERS`-adjacent, one material
  bit in the packed vertex) that makes the shader skip the sun/shadow term — fullbright ink. Torch flames, lantern
  glass, and the Aetherlamp glow in the dark; Glowvein and Aetherock ore seams get the same bit, so revealed veins
  glitter in dark caves (a free Mining synergy). Cheap: no new passes, no light field. Limitation stated plainly: a
  Stage-A torch is visible in the dark but does not illuminate its surroundings — it's a way-marker, not a lamp.
- **Stage B — LANDED (see LIGHT-DESIGN.md): true voxel light.** Torches cast a real flood-fill blocklight field and a
  full skylight column channel darkens caves and roofed rooms. One correction to this section's original sketch:
  propagation is NOT "via the padded shell" (a light of level N reaches N cells and the shell reaches one) — the field
  is stored per chunk beside the store and the shell only carries settled values into mesh jobs, the same role it plays
  for AO. `BlockProps.light`/`emits(level)` is the source knob (the Torch's level is set on its own row; Lantern and the
  Aetherlamp slot in as rows of their own), independent of `emissive` — ore seams still get ink that casts nothing.

Torches are floor-mounted in v1 (one shape, no facing states); wall mounting is a state nibble away when wanted (§12).

Still deliberately out of v1: container blocks (chests — the inventory is already generous) and mechanisms.

### 7.5 Nothing the world hands you may be transmuted

The bench exists to make what a fresh world does not give away. Dirt, sand, snow, grass, mud, the felled log and the
scooped bucket charge are all things you walk over and pick up, and a recipe for one is the bench competing with a
shovel — so the catalog is gated on a single derived predicate, `BlockType::is_bench_only`: *can no dig in an untouched
world put one of these in a player's hands?* `unlock::craft` and `alchemy::craft_ideal` both open on it, and
`nothing_the_world_hands_you_can_be_transmuted` pins it against the hand-priced table as well as against the gate, so a
retired price cannot sit unreachable in `functional_ideal` the way the bucket charge's did.

Its companion is the XP half: **extraction pays for reducing what the world made, and nothing for undoing your own
work.** `skills::convert_xp` answers zero for anything bench-only. Without it, craft-then-extract was a standing
converter from essence into levels — you were paid once for making a thing and again for unmaking it. The bench's PACE
deliberately does not follow (`alchemy::extract_worth`, split out of `convert_xp` for exactly this): a lumber block is
as much substance to take apart as the log it was planed from and must come apart just as slowly.

**Clean rock is the exception the rule is built to allow, not one carved out of it.** The world is *made* of stone and
still never hands you one, because a quarried stone arrives as Rubble — so §7.1's whole economy passes the same
derivation as everything else. Making that true took the conditional half of `crumbles`: a stone somebody **placed**
hands itself back rather than crumbling, keyed on whether the cell has ever been written (the journal on the server, its
`EditOverlay` mirror on the client — the same bit §5.3's reveal already reads, and for the same anti-dupe reason). Were
a bench stone to crumble like the mountain's, craft → place → dig → extract would launder a made block into a
world-given one and collect the XP for it.

Below the mason's own unlock a placed stone crumbles anyway, and that is not a punishment. A player who could not have
made one is not undoing their own work, so they get the rubble and the XP that comes with it — and the loop still cannot
close for anybody, because the levels that can craft a stone are exactly the levels it comes back whole to.
`no_one_who_can_make_a_block_can_dig_it_into_xp` is the pin, asked at every level that can craft the block.

Two consequences worth stating rather than discovering. A high-level mason can lay clean stone for a beginner to break
for XP — it costs the mason more essence than the digger recovers, needs an accomplice, and only helps someone early, so
it is priced rather than exploitable. And **crafting still pays**, so craft-then-extract remains a slow essence-into-XP
grind at half its old rate; whether transmuting should pay at all is a separate question about §4.1, not a consequence
of this one.

---

## 8. UI

### 8.1 Alchemy screen (total redo — SHIPPED; the mock below is the as-built layout)

The old single linear list died. Structure — same `Row`-stream architecture, theme tokens, one glyph size, `fit_scale`
ladder, everything on ONE left column, and the panel height is the max of both tabs so the bottom-anchored top edge
never jumps on a tab switch (`panel_height_is_the_same_on_both_tabs`):

```
┌──────────────────────────────────────────────────────────┐
│ ALCHEMY   LV 41                     [██████████░░░░░░]   │  header: level + XP bar
│ FLORA 356   EARTH 1,204   AQUA 40   METAL 88   AETHER 2  │  essences (wire order, always visible)
│ ┌─────────┐                                              │
│ │ EXTRACT │  TRANSMUTE                                   │  real tabs: active fill fuses into
│─┘         └──────────────────────────────────────────────│  the page; the rule breaks under it
│ ┌────┐                                                   │
│ │⧉ 214│  → nn EARTH   ·   1 block / PACE       [🪣 tile] │  extraction slot (count badge +
│ └[██░]┘                                                  │  progress inset) · preview, elided
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦                                       │  before the bucket tile
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦   the real inventory grid             │
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦   (same widget, drag sources)         │
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦                                       │
└──────────────────────────────────────────────────────────┘
```

(TRANSMUTE swaps the strip+grid for: family tiles → member tiles — both wrapped at the inventory grid's column count,
locked tiles dimmed with a top-left unlock badge — then the `ACTION_ROW_H` detail strip with the ONE TRANSMUTE button.
Hover washes every interactive tile; the selected family wears the WEAK ring, the acted-on member and an armed
extraction slot the STRONG one — two weights, one containment hierarchy.)

The EXTRACT tab is **your inventory plus one drop target** — the player picks the stack, never the screen. Rules, each
load-bearing:

- **Source is the real inventory grid** (and hotbar), the same widget, slot rendering, and `DragState` the inventory
  screen already uses. Drag a stack onto the extraction slot to begin; the yield preview and per-block pace draw beside
  it, at your level. The pace is the BLOCK's, not just the level's: `extract_pace_ticks(block, level)` scales the
  level's whole-block interval by the block's `convert_xp` against a whole block's, floored so the cheapest foliage
  doesn't become a wire-op stream. Keying it on the XP table rather than a second table is what stops pace and pay
  disagreeing about which blocks are the substantial ones.
- **The extraction slot is a pointer, not a container.** It holds a selected block id; the stack never leaves the
  inventory, so the slot touches neither `reconcile_layout` nor the `Layout` wire — it is screen-local UI state, cleared
  when the screen closes. The grid cell visibly drains in place as the drip consumes it.
- **The drip is client-paced ordinary ops.** While the slot is occupied, a progress bar fills over the per-block period;
  each completion sends a normal `AlchemyOp::Convert { count: 1 }` down the action queue, the server answers with the
  standing `InventoryFull` rebase, XP drips per §4.1. **No new wire, no server-side machine state** — the server never
  learns the slot exists, and offline worlds run the identical rules through `apply_alchemy`. The base pace (see below)
  sits far under the shared edit token bucket (`EDIT_RATE_PER_SEC`), so extraction never starves edits.
- **But the pace itself is server-enforced** — client pacing is presentation, not trust (the return-to-spawn precedent:
  the server holds the clock). `Convert` ops draw from a per-player **extraction bucket** alongside the general edit
  bucket. The bucket banks TICKS — one per tick, capacity a small multiple of a whole block's worth
  (`extract_burst_ticks`, `EXTRACT_BURST`) — and a `Convert` spends the requested block's own
  `extract_pace_ticks(block,
  level) × count`, so neither the legacy bulk-Convert path nor a stream of cheap blocks can
  sidestep the drip. An op arriving short refuses cleanly through the standing `InventoryFull` rebase: nothing debited,
  no XP, and the honest client self-heals for free (the refused block is still in the count; the next cycle re-sends).
  The burst absorbs network bunching exactly as `EDIT_BURST` does. Unlike the spawn cooldown, nothing persists — the
  window is short enough that rejoin-to-reset buys a blink and isn't worth a `PlayerRecord` field. The gate lives in
  server op validation like the edit bucket does, NOT in `apply_alchemy` — pacing is anti-cheat, not an economy rule,
  and offline worlds keep running the identical economy.
- **Pace**: one block per interval, the interval shortening at two points on the Alchemy ladder
  (`alchemy::base_pace_ticks`; these *replace* the old batch-cap unlocks). A big hoard is now minutes at the bench
  instead of a click — the RS-style "stand at the bench and watch the XP drip" is deliberate.
- **Stopping**: drag the stack out, click the slot, close the screen, or run the count to zero — the in-flight cycle
  simply never sends its op. Ops are atomic per block, so there is no partial debit to unwind, ever.
- **The bucket charge appears as a single water block** beside the grid whenever the tool is charged (it can't live in
  the grid — `is_slottable: false`), and extracting it **empties the tool**: the charge *is* the inventory's Water
  count, capped at one, so the ordinary `Convert` debit is the emptying — no special case anywhere, and the held
  bucket's geometry (`HELD_TOOL_FULL` → empty) follows the count like it already does after a pour. One drag, one cycle,
  the Aqua paid out, tile gone.
- Extraction runs **only while the screen is open**. No AFK-extraction-while-mining; you stand at the bench.

TRANSMUTE is structured as **families with one button** (reworked to this shape during the build, on feedback that the
flat row list didn't read): a row of FAMILY tiles — SLABS, STAIRS, DOORS, BLOCKS, … — each slowly rotating its icon
through its members (wood stairs, stone stairs, …) on a shared clock; selecting a family lays its member tiles out
beneath (locked members visible but dim, affordable ones badged with their batch count); a detail strip shows the
selected member's name and post-waste cost at your level; and the tab has exactly ONE `TRANSMUTE` button, disabled (no
hit rect) for a locked or unaffordable pick. `CLICK 1 · SHIFT 10 · CTRL
100` multipliers on that button.

That button **queues** its count rather than buying it. A batch drips one paced craft at a time exactly as a stack drips
one paced extraction, the row beneath carries the batch's progress bar and what is left of it, and the button stands as
its CANCEL — the same shape clicking the occupied extraction slot has. It used to answer instantly, which was the last
place in the game where XP arrived in a click (§4.1). Like extraction, a batch runs **only while the screen is open**,
and the two never run at once: leaving a tab abandons its drip, and the server's one bench bucket says the same thing.
This is also the §7.2 grid's natural UI: a new material × shape pair is one more member in an existing family, so the
catalog grows without the screen growing. Tools/essence-exchange rows join as families when they land (M4+). `fit_scale`
remains the overflow fallback.

### 8.2 Character screen

New screen, `C` key (new `Key` variant) + a pause-menu row + `DIG_SCREEN=skills`:

```
┌────────────────────────────────────┐
│ CHARACTER                          │
│ MINING    Lv 47  [████████░░] 61%  │
│           next: Lv nn — Glowvein   │
│ ALCHEMY   Lv 41  [██████░░░░] 44%  │
│           next: Lv nn — Pillar     │
│ TOTAL     88                       │
└────────────────────────────────────┘
```

One row per skill: level, XP bar to next, next-unlock teaser — SHIPPED, with the teaser prose screen-local and every
named level pinned to the core function that enforces it (`teaser_levels_match_the_core_tables`). Mining's teasers are
the §5.1 rock-gate rows until ore lands (past the last rock rung the line simply drops). Clicking a row could later
expand the full unlock list; v1 is the summary.

### 8.3 Feedback

- **XP toasts**: a small accumulating `+N MINING` above the hotbar, HUD-layer text over a `FILL_BG` backdrop (sized from
  `line_h`), coalescing rapid awards and fading out on their own.
- **Level-up**: chat-feed line ("Mining is now level 48"), a one-shot sfx, and on reveal thresholds the remesh sweep
  plus a distinct line ("new veins shimmer into view").
- Both new screens get `DIG_SCREEN` stocking (essences + ores + XP included) so `--screenshot` can photograph them — the
  two shipped look-and-feel regressions both came from screens invisible to headless capture.

---

## 9. Wire, persistence, migration

**Protocol v15 → v16** (append-only as always):

- `ServerMsg::Skills { xp: Vec<(u8, u64)> }` — full sync, sent at join after `Welcome`.
- `ServerMsg::XpGain { skill: u8, total: u64 }` — new *total* (idempotent, reorder-safe), sent on award; the client
  animates the delta itself.
- No new `ClientMsg`. `AlchemyOp::Convert/Craft` shapes unchanged — the server-side tables behind them change meaning.
  `EditRequest` unchanged (ore extraction is derived, §5.3).
- Essence ids: the `Material` enum becomes `Essence { Flora, Earth, Aqua, Metal, Aether }` riding the same
  `SUB_WIRE_FLAG | index` pairs. **The migration is a numbering choice, not code**: Flora keeps sub-wood's index and
  Earth keeps sub-stone's, so every record and save written under the two-family economy decodes 1:1 with zero migration
  paths anywhere (flagged pairs carry no version, so index-compat is the only migration that can't misfire). The
  water-stack precedent holds: a legacy count decodes to the new currency, never a windfall, never wiped.
- `SIM_VERSION 10 → 11`: level-scaled dig timing, ore hardness rows, DepletedStone.

**Persistence**:

- `inventories.bin` v3 → **v4**: `PlayerRecord` gains `skills: Vec<(u8, u64)>`; v1–v3 read-compat (skills default empty
  = level 1). Existing players keep their inventories and start at level 1.
- `state.json`: a `skills` field with `#[serde(default)]` — same policy.
- The journal is **untouched** in format and version; DepletedStone/placed-ore cells are ordinary edits.

**Server hooks** — both are single existing chokepoints:

- Dig credit (the block in `edits.rs`): award Mining XP, run the ore derivation (write DepletedStone + credit ore + dual
  XP), then the existing `reconcile_layout` + persist.
- `process_alchemy`: award Alchemy XP alongside the existing `InventoryFull` answer; append the `XpGain` frame on the
  same reliable stream (in-order with the rebase).

---

## 10. Test plan (the pins that make this safe)

- `xp_curve_table_is_pinned` — golden cumulative table.
- `no_level_ever_creates_matter` — for every block × every level: total extraction yield < total craft cost;
  efficiency/waste monotonic and bounded.
- `bonus_lines_never_sit_on_craftable_blocks` — the §3.3 pump rule: every bonus line lives on a recipe-less block, so no
  craft-extract loop manufactures Metal or Aether.
- `dig_speed_anchors_are_pinned` — every family's two endpoints hold, Fixed never scales, and `required` is monotonic
  non-increasing throughout. (The mid-curve anchor this originally pinned — one level reproducing the legacy hardness
  table exactly — was retired at SIM v18; see §5.1.) `every_dig_speedup_is_smaller_than_the_one_before` pins the taper's
  shape alongside it.
- `dig_families_are_pinned` (explicit block → family roster, covering every diggable block) and
  `rock_is_gated_until_its_level` + the server half (`gated_rock_refuses_the_dig_start` — refusal at DigStart, no hold
  opened).
- `ore_positions_are_pinned` (golden per-seed, like `spawn_positions_are_pinned`) + density-histogram bounds per
  tier/depth band + `ore_never_manifests_in_edited_cells`.
- `field_extraction_writes_depleted_never_air` and the dupe test: place Stone into a pristine deposit cell → no ore.
- `mined_rock_is_always_rubble` — every rock-family `crumbles` points at Rubble, and the rubble→stone loop is a sink at
  every level (what the extraction hands back is always less than what the craft costs).
- §7.5's four, which are one rule read from four sides: `nothing_the_world_hands_you_can_be_transmuted` (no dig in an
  untouched world hands out something with a recipe — checked against the hand-priced table too, so a retired price
  cannot hide behind the gate), `no_one_who_can_make_a_block_can_dig_it_into_xp` (the loop-closer: at every level that
  can craft a block, digging one back pays nothing), `extraction_xp_is_paid_for_what_the_world_made` (and the pace still
  is not), and `only_the_worlds_own_rock_crumbles` (the conditional drop is opt-in per row, level-blind when pristine,
  and `dig_drop` stays the pristine answer every economy pin means). The server pair
  `a_placed_stone_comes_back_whole_and_a_quarried_one_crumbles` / `below_the_masons_unlock_even_a_placed_stone_crumbles`
  walks it through the real edit path, where the journal is the authority for "placed" and has to be read before the dig
  writes its own entry.
- `grid_gates_compose` — every authored material × shape pair unlocks at exactly `max(shape level, material level)`; no
  curated stragglers.
- `extraction_pace_is_server_enforced` and its transmute twin `transmuting_draws_on_the_same_bench_bucket_as_extracting`
  — ops beyond `pace(item, level)` refuse with no debit and no XP; sustained throughput of a flooding client ≤ honest
  throughput at the same level; `count > 1` charges the bucket per item, not per op; and a craft spends the ticks an
  extraction would have used, because there is one bench. `the_bucket_can_always_afford_the_costliest_op` covers the
  other end: an op no full bucket can hold is an op refused forever, online only.
- Reveal remesh: crossing a threshold bumps resident generation counters exactly once.
- `every_screen_draws_text_at_one_size` / `..dims_and_panels_the_same_way` extended to the character screen and the
  rebuilt alchemy screen (every window size × scale pair those two sweep).
- `panel_height_is_the_same_on_both_tabs` (the bottom-anchored panel's top edge never jumps on a tab switch),
  `every_craftable_appears_in_exactly_one_family` (the screen's `FAMILIES` mirror can't silently miss a new
  `unlock::craft` entry), and `teaser_levels_match_the_core_tables` (§8.2's prose pinned to the enforcing functions).
- Loopback-transport e2e: XP totals survive rejoin (v4 records), legacy v3 record joins at level 1 with inventory
  intact, legacy sub-materials decode to essences.
- Netbench determinism: XP awards excluded from per-frame hashing or made order-stable (awards ride the action queue in
  wire order, so they should already be).

---

## 11. Milestones

Each milestone ships alone and is worth having alone. Two dependency inversions the first sketch of this section got
wrong: field extraction needs the **Essence Siphon**, and the draw effect needs the **Stage-A emissive bit** — so the
tool machinery and the shader bit both move *ahead* of ore, and "catalog + tools" is no longer one phase. Each entry
lists the follow-on work the feature drags in that the design sections above don't spell out — art, audio, shaders,
parity — because that's where the real schedule lives.

**M1 — Skills skeleton** *(medium)*. Curve + `Skill` enum in core, wire v16, dig-credit and `process_alchemy` XP hooks,
`inventories.bin` v4 + `state.json`, XP toasts, character screen. Mining XP from ordinary digs only; zero balance
change. Hidden work: the `C` key is a new `Key` variant, which ripples through winit translation in `digblock-platform`,
the web client's keydown mapping, *and* a pause-menu row (iOS has no keyboard — the menu row is the touch path, not a
nicety). The character screen must join the two pinned theme tests and get `DIG_SCREEN=skills` stocking, or it ships
invisible to capture like the two regressions before it. The level-up sfx is the **first new audio asset**: CC0
sourcing, provenance in `SOURCES.md`, the oggenc pipeline, embedding, and by-ear verification in *both* fillers — audio
has no headless test. Netbench needs the XP-award-ordering check (§10).

**M2 — Essence economy + Rubble + the alchemy screen redo** *(large — the economy switches here)*. `Material` →
`Essence` with decode-time legacy migration, extraction / waste / bonus-line tables, the Rubble rule, the rebuilt
two-tab screen with the extraction slot, and the server-side extraction bucket (§8.1). Hidden work: this milestone
contains the project's **one destructive test change** — the exact-closure economy pins (a block's worth of slabs
extracting back into exactly one block) are retired and every cost test regrows around the waste multiplier; budget for
a test-suite migration, not just new pins. Rubble is the first new art (decide hand-authored vs. `texgen`-derived). The
screen is the largest UI job since the inventory: a progress-bar widget that doesn't exist yet, drag-to-slot on
**touch** (no hover states), captured-wheel scroll, locked/OWNED rows, theme-test membership, `DIG_SCREEN=alchemy`
re-stocked with essences so captures show the new tabs. The per-cycle extraction tick wants a small sfx (audio pipeline
again). Loopback e2e for v3→v4 records and legacy sub-material decode lands here, because this is the milestone that
could corrupt somebody's stash.

**M3 — Mining speed** *(small)*. Level-scaled `required`, SIM 11. Hidden work: dig completion ticks shift with level, so
netbench re-baselines — the determinism gotcha already on record. `dig_grace` absorbs mid-hold level-ups; verify with a
test, not by assertion. Note this is the plan's **one deliberately player-visible nerf**: §5.1's below-baseline start
slows every existing player down on day one, climbing back to familiar speed as the ladder is climbed — it ships *after*
M1's XP system so the paying-back is already running when the debt lands, and the patch note should say so in exactly
those terms.

**M4 — Tool formalization + Stage A light + Torch** *(medium — pulled ahead of ore, which needs both halves)*. The
separated tool slot, tap-9 cycling, the faded rail, tool-id migration of the `Layout` tool entry; the emissive material
bit; the Torch as its first user (it sits pre-ore in the level table for exactly this reason). Hidden work: the emissive
bit is **cross-shader** — the material word's upper bits are shared with the water payload, so the bit must be carved
where the opaque path actually has room, and chunk/shadow/water.wgsl must keep agreeing (the naga test catches invalid
WGSL, not wrong-looking light — screenshot captures are the visual gate, and Tint's stricter rules mean the web build
must be checked, not assumed). Decide shadow behavior explicitly: emissive skips the *received* sun term but still
casts. Stored layouts in `player_store` carry the old `WIRE_TOOL` sentinel — the migration is decode-time there too.
Cycling needs its touch rule verified on device, and the rail icons ride the existing `item_icon` sites. Torch is a new
baked shape (floor mount) with an emissive flame layer: shapes.rs authoring, `MAX_SHAPE_QUADS` pin, one PNG.

**M5 — Ore** *(large — the headline)*. `ore_at` + golden pins + density histograms, five block rows, reveal stamping +
threshold remesh, the Siphon, field extraction, DepletedStone, dual XP, the draw. Hidden work: four ore textures +
DepletedStone with **emissive seam layers** designed to read at rest *and* under progress-driven bloom — the most
art-directed assets in the plan. The draw itself is three new render pieces: a world-space mote emitter (the `spray.rs`
mold, but a new instanced pipeline), the siphon viewmodel in `item_geom` as the convergence point, and **per-cell
progress-driven emissive**, which has no existing plumbing — the crack-decal path is the nearest precedent and it's a
decal, not a material modulation; expect real design here. Remote players seeing the stream means the wire must say *who
is currently drawing on which cell* — the server knows (DigStart), but today no snapshot field carries an active
dig-hold; that's a small append-only addition the doc's "no new wire" claim for extraction does **not** cover. Audio:
the siphon draw is a *sustained loop*, and the audio system is one-shots + ambient beds — the waterfall-voice
reconciliation is the only precedent for a held voice; plus a completion burst and a refusal thunk for `DigRefused`. The
reveal remesh sweep needs its once-only pin, and the mesher snapshot grows a mining-level field (plumbed like
`EditOverlay`, wasm-inline path included).

**M6 — Resonant Pick + endgame lines** *(small-medium)*. The pick (begin_multi reuse, server-side pair derivation, the
level-only timing multiplier, the later reach extension), prospector's sense, the mastery line at the cap. Hidden work:
prospector's sense is a new *cadence audio emitter* driven by an `ore_at` proximity scan — pure and cheap, but it's
another sourced sfx and another by-ear check. The pick's second/third dig cell interacts with protection and
`InsidePlayer` per-cell; the assembled-writes list covers it, but the test has to prove that, not cite it.

**M7 — Catalog waves** *(open-ended, ships release by release)*. Materials and shapes per §7.2's grid, functional items,
glass, the lights. Hidden work — this is where the follow-on tail is longest:

- **Every new shape is a shapes.rs project**, not a row: sub-box authoring, baked variants, `FaceMask` coverage,
  collision + interaction boxes, `state_valid` gates, a `MAX_SHAPE_QUADS` pin, facing tests. Trapdoor adds a **new
  facing axis** to the state nibble; Ladder needs the first new movement rule since swimming (§12 — ship decorative
  first or hold it); Wedge reuses ramp physics but must prove it.
- **Art is the long pole.** Counting §7 as written: Rubble, four ores, DepletedStone, Torch, Lantern, Aetherlamp, Framed
  Window, Glass, Stone Bricks, Polished Stone, Sandstone Bricks, Metal, plus a PNG per authored material × shape pair —
  dozens of textures, with `texgen` extended for the derived ones. The grid rule means the *code* scales; the art never
  does.
- **Audio material tables**: metal, glass, and rubble footsteps/dig sounds don't exist — every new block family needs a
  `materials` mapping in `audio.rs` and sourced sfx, or new blocks are silent in a way players notice immediately.
- **Glass** ships `Cutout` per §12's answer; revisiting translucency is a render design question with the water pass,
  not a catalog task.
- The essence exchange and the Water craft are each likely **one append-only `AlchemyOp` arm** (essence→essence and
  essence→charge don't fit `Convert`'s block-in shape) — small, but wire-visible, so they belong to a planned protocol
  bump, not a patch.

**Cross-cutting, every milestone**: wasm-clean core changes (`just check-web`), `just check-ios`, touch input for every
new interaction, `DIG_SCREEN` stocking for every new screen state, and a CLAUDE.md update as each phase's invariants
land. Balance is a workstream, not a milestone — every number above is a proposal, so plan a bot-soak + playtest after
M2 (the economy) and M5 (ore rates) specifically, since those are the two milestones that set prices players will
remember. Every value the balance work moves lives in `skills.rs`, `alchemy.rs`, `unlock.rs` and `material.rs`, and in
nothing else — this document names them and never restates them, so a retune is a code change and not a documentation
sweep.

---

## 12. Open questions

- **Ore names.** Emberore/Duskstone/Glowvein/Aetherock are placeholders; four names carry a lot of the fantasy and
  appear in inventory, alchemy rows, and unlock text.
- **Essence display name for Metal** — "Metal" vs "Mineral" vs leaning into "Ore" despite the raw-ore collision.
- **Ladder physics** — climbable needs a new movement rule (the first since swimming); ship the shape as decorative
  first, or hold it for the physics?
- **Should placed ore blocks be diggable by below-gate players?** Current answer: yes, placed ore is ordinary chunk data
  (visible to all, drops itself). The alternative — reveal-gating placed ore too — breaks "chunk data is what everyone
  sees" for marginal gain.
- **Extraction queue depth** — one slot in v1; the natural high-level QoL is a second queue slot (finish this stack,
  start that one) rather than any "extract all" button, which would hand the picking back to the screen — the thing the
  slot model exists to avoid. §13 schedules it as a mid-ladder Alchemy unlock (QoL-as-unlock).
- **Curve steepness** — RESOLVED by §13: the curve is **frozen forever**. Level is derived from stored XP, so steepening
  `XP_TO_NEXT` would retroactively lower every existing character's levels — a betrayal no retune is worth. Pacing is
  tuned on the AWARD side (`dig_xp`, `convert_xp`, `ore_field_xp` — which only slow future earning) and by where
  thresholds sit, never by repricing levels.
- **Glass render class.** `Translucent` currently means the specialized water pass (surface payloads, its own sorting
  story); glass through that pass is unproven. Safer v1: `Cutout` with a framed/latticed texture that reads as glass
  without per-pixel alpha blending; upgrade to true translucency when the render question is answered on its own terms.
- **Rubble scope.** Stone and Sandstone share one Rubble id (one texture, one row). If sandstone rubble wants its own
  warm-toned look later, a second id is append-only — but one rubble keeps the extraction table and the cobble aesthetic
  simple.
- **Torch mounting.** v1 is floor-mounted only (one baked shape, no facing states). Wall torches are a state-nibble
  variant like slab anchors when wanted.
- **Stage B lighting** — RESOLVED: landed as LIGHT-DESIGN.md (blocklight + skylight, §7.4). M5's ore-seam glitter still
  uses only the Stage-A ink bit, as designed.

## 13. The 1–60 compression (design review, 2026-08-16)

A full design review (progression cadence, economy, theme fit against "RuneScape-y in a voxel world") restructured both
ladders: **everything the game currently offers, and everything §7.2 still plans, lives in levels 1–60.** The cap stays
where it was; the stretch above the content is deliberately empty headroom that future expansion waves fill, and players
may bank levels into it ahead of the content — a wave that lands at 62 pays out instantly to whoever already climbed
there. Two companion documents came out of the same review: `DELVE-DESIGN.md` (underground dungeons, hazards, the
Delving skill) and `SOCIAL-DESIGN.md` (hiscores, mastery items, claims, trade).

### 13.1 Why compress

The review's verdict, in brief. What works and stays the constitution: the no-RNG threshold philosophy (a guaranteed,
visible, dated reward is the competence-respecting version of loot — self-determination theory's reading — and the
goal-gradient effect works *better* when the distance to the next threshold is exact), the material × shape grid as a
content multiplier, the cross-skill handshakes (siphon before the first reveal, the pick's two-skill gate), and
design-as-tests. What failed: reward cadence collapsed exactly where the curve turns expensive. The curve doubles every
ten levels, so the old ladder's long empty stretches (Mining's nineteen-level gap between the pick and the last reveal
was the worst) sat precisely where each level costs the most — flow theory's boredom channel, challenge rising while
novelty flatlines. Meanwhile fanfares that celebrated non-events (a door shape years before any door, a metal dig gate
for a block no one at that level could own) taught players to discount fanfares, and the endgame's best essence loops
had regressed to farming first-rung blocks.

Compression fixes cadence without touching the curve: with the same thresholds packed into 1–60, the longest quiet
stretch in the content range is a handful of levels, and the priciest unlocks sit at the top of the range where the
curve has already doubled twice — threshold *placement* doing the pacing work that the curve must not.

### 13.2 What moved (landed with the review)

The authority is `unlock.rs` / `alchemy.rs`; this list records the *decisions*, not the numbers:

- **Every material, reveal, functional item and bonus line was remapped into 1–60.** Roughly: the first fifteen levels
  kept their shape (they were already dense), and everything above compressed by about half, nudged so that cross-skill
  pairs land together and no two events collide. The pinned `unlock_levels_are_the_threshold_levels` lists are the
  remapped ladders.
- **The door shape now lands WITH the first door-bearing material** (`shape` and `material` agree), so the shape fanfare
  and the first craftable door are one event. The old gap was the review's canonical "celebrated non-event".
- **The Metal and Crystal strength gates equal their materials' own craft levels** (`mine_material` mirrors `material`,
  pinned in `the_strength_wall_is_the_dig_family`). A strength gate below the craft level was provably dead weight —
  `pickup` already refuses the drop — and its fanfare celebrated a block the player could not obtain.
- **The prospector's sense grew a second rank** (`prospect_range`): it arrives faint just past the first reveal and
  sharpens in the stretch before the deepest one. Both ranks are manufactured near-goals — the sense literally tells the
  player "something is within reach" during the levels that otherwise have no threshold in sight.
- **Ore mastery crowns the content ladder, not the cap** (`ore_mastery`): the last reveal's stretch ends at mastery, and
  the level-cap line above it belongs to the tail policy below.
- **The endgame Aether bonus lines moved onto the deep tiers' spent rock** (`bonus_lines` on DepletedGlowvein and
  DepletedAetherock — "the spent stone's whisper"). The old lines sat on gravel and pine leaves, so the optimal
  top-of-ladder loop was farming blocks a novice digs; spent deep rock is what a late-game miner's hands are already
  full of, so the apex reward now attaches to the apex verb. Pump-clean by construction: spent rock is only ever written
  by a siphon draw, drops itself, and nothing craftable drops into it. Gravel keeps its mid-ladder Metal line.
- **The bench pace steps and the waste clamp compressed with everything else**, and the waste clamp's dip now lands just
  past where the catalog tops out — announced in the teasers rather than silent, which it was.

### 13.3 The tail policy

The stretch between the content's top and the cap is **near-empty on purpose**, and the three things in it are each
there by decision:

- **The linear efficiency and waste curves keep running to the cap** (`scaled_yield`, `waste_permille`), so every
  post-content level still pays a small, real, announced gain. The level-up feed should say so (the character screen's
  efficiency readout is the surface for it).
- **The pick's full plane stays deep in the tail** (`pick_cells`' last rank, and the carried-light step that rides it) —
  the owner's one deliberate exception to the squish: a single long-haul functional goal out past the catalog,
  RuneScape's "the last stretch carries the prestige" in one unlock.
- **The Master's Sigil stays at the cap.** It does nothing, it costs everything, and that is the point — it is the
  skillcape. Giving it a function would make it a tool with a price rather than a trophy.

Future waves land IN the tail (and new early content squeezes into 1–60's remaining seams). A player who banked levels
cashes them in the moment a wave lands; that is accepted, and it is also the retention argument — the tail is a promise
that the climb will keep paying.

### 13.4 Pacing (the workstream)

Progression is currently faster than intended, and compression packs more into fewer levels. The standing rules:

- **The curve is frozen** (§12's steepness question, resolved). Pacing levers, in preference order: the award tables
  (`dig_xp`, `extract_worth`/`convert_xp`, `ore_field_xp` — `dig_xp`'s scale-down has now been halved twice, with
  foliage and the torch zeroed by name, while the field table deliberately held still), the depth multiplier's ceiling,
  `PICK_EXTRA_PERMILLE`, and the bench pace (a slower bench is slower Alchemy XP/hour without touching an award). All
  award-side: they slow future earning and deflate nobody.
- **The dig-to-vein gap is the design, not a drift.** Both halvings widened how far a field draw out-pays a plain dig,
  and that ratio is DELIBERATE: finding ore should feel rewarding — digging is the road, veins are the pay. A future
  balance pass may move either table, but it may not "correct" the gap toward parity; a vein that pays like rock makes
  the reveal levels mean nothing.
- **The background bench.** Alchemy's slowdown came as a pair, and the pair is the design: the bench pace quadrupled AND
  the bench keeps working with its screen closed — a job runs while you play, only leaving the world abandons it, and
  `hud::bench_tell` keeps the working bench visible. One without the other fails: a 4× pace attended is idle time, and a
  background bench at the old pace is free levels. Because XP-per-bench-tick is a constant by construction (pace and
  award both derive from the op's worth), the passive rate is ONE legible number to tune. It runs only while IN a world,
  never logged off — an offline accrual would make the pace meaningless. Bench XP also halved (`convert_xp`/`craft_xp`
  pay half the op's worth; the worth itself is untouched, since it still drives the pace), and **foliage is the
  deliberate exception in both directions**: zero Alchemy XP, half the slowdown, one shared predicate
  (`BlockType::is_foliage` — the same set the dig table zeroes). The two halves of that exception must move together — a
  floored one-point award against the concessionary pace would make shoveling leaves the best XP rate at the bench.
  Foliage is pure economy: the Aqua and the bonus lines are what a leaf is for.
- **Design to time budgets, then measure.** Intended bands: the first reveal inside the first session; the siphon
  shortly after; the pick a multi-session goal; the content top a long-haul goal measured in weeks; the plane and the
  sigil in months. These are claims about hours, so they are testable.
- **Instrument before retuning further.** The bot harness (`just bot`, `just compose-bots`) plus the metrics rail can
  log XP/hour per activity (surface digging, deep mining, bench extraction, crafting); findings go in the BENCH.md style
  — dated, superseded rather than edited — so the next award retune argues from measured rates instead of feel. Balance
  was always "a workstream, not a milestone"; this gives the workstream numbers.

### 13.5 Content that fills the remaining seams (design, not built)

The compressed ladder still has thin stretches, and §7.2's machinery is how they fill — each item below is a tuning
proposal in the header's sense (the level is final only when it lands in `unlock.rs`):

- **The shape wave** (each a `shapes.rs` project, cheapest geometry first): Fence ~14, Ladder ~20 (decorative first,
  §12's physics question unchanged), Pillar ~26, Roof Wedge ~32 (must prove ramp-physics reuse), Pane ~38, Gate ~44,
  Trapdoor ~48 (a new facing axis — hardest, last). Every shape retro-fans across owned materials on arrival, which is
  the grid doing the content-multiplying it was built for.
- **Grid hole fills** (a `blocks!` row + PNG each, costs derived): Glass stairs/door/pane; Crystal stairs/pillar/pane —
  Crystal across more shapes is the repeatable Aether sink the top of the ladder wants, since its craft cost is an
  Aether sink by construction; PolishedStone and SandstoneBrick doors; Grass and Snow slabs, which redeem the
  currently-inert material levels (a grass-slab lawn is a genuinely wanted builder block).
- **New functional items** (hand-priced per §7.3, payability pin applies): a Framed Window ~16 (the pre-Glass window,
  already sketched in §7.3), a Brazier ~38 (a standing Metal-and-Flora firelight between the lantern and the
  aetherlamp), an **Aetheric Alloy material ~52** (a mostly-Metal, some-Aether whole-block split in `material.rs` — the
  last material, the repeatable prestige building block, and the reason Aetherock mining is standing income), a
  Chandelier ~54 (the showpiece light, the biggest repeatable Aether sink).
- **The second extraction queue slot ~42** (QoL-as-unlock; §12's answer, scheduled).
- **The craftable bucket — BUILT** (protocol v25): the bucket lost `always_owned` and is the catalog's first tool, gated
  by its price alone (Flora and Earth, at the level everyone starts at, so the opening dig-and-extract loop pays for it
  — "make your first vessel" is the game's first craft beat). Its price carries **no Aqua** (water is how a player first
  GETS Aqua — the siphon's no-Metal deadlock rule, applied again). The Water extract gate dropped to the first level
  with it, and the `extract` axis — a one-entry table by then — was deleted outright; if a substance ever genuinely
  needs its own extraction level, the axis returns with more than one row in it. A fresh player's tool slot stands BARE
  (`Hotbar::active_tool` is an `Option`; `tool_in_hand` spells the empty hand as `Air` for the shared dig questions;
  reconciling auto-equips the first crafted vessel). Every record written while the bucket was owned by everyone keeps
  it via a migration grant — `inventories.bin` v7 server-side, `SAVE_REV` for local saves, with a fresh world opening AT
  the current revision so the grant can never be a windfall.

### 13.6 Beyond these ladders

The review's larger findings — that the game's missing half is its *sociology* (visible, comparable achievement), and
that the world needs a place that pushes back — are designed in the two companion documents: `SOCIAL-DESIGN.md`
(hiscores through the master, per-skill mastery items in the sigil's mold, player claims, why trade waits) and
`DELVE-DESIGN.md` (underground ruins that heal themselves through ordinary journaled edits, deterministic telegraphed
hazards instead of RNG combat, the Delving skill, and the governed Aether faucet). Their sequencing lives there; the one
ordering rule worth restating here is that the social mirror ships first, because every content hour is worth more once
someone can see it.

## 14. The essence redesign — essence is found, not ground (2026-08-18)

The owner's verdict on the shipped economy: Metal and Aether felt right and the rest did not. The difference was not the
numbers — it was WHERE the currency came from. A vein is a place: derived, hunted, revealed at a level, drawn as an
occasion. Earth, Flora and Aqua were verbs: put the block you were already mining in the extract slot and hold still.
Supply was unbounded, so every common-essence price was really a statement about time at the bench, and the Alchemy XP
attached to extraction was a tax paid by standing there. §13.4's pacing work slowed that faucet without closing it,
because the problem was the source, not the rate.

The redesign is one principle applied everywhere: **essence should be found, not ground.** Every essence now has world
sources (nodes) on exactly ore's machinery — salted hashes or pure worldgen queries, manifesting only in pristine
unedited host cells, revealed by a skill level in the mesh stamp, drawn in place with the siphon, leaving a spent face
that drops itself, anti-duped by the journal's monotone edited bit. Retroactive in existing worlds, no worldgen bump,
golden hashes untouched.

### 14.1 What landed

- **Bench extraction is GONE.** `AlchemyOp::Convert` is a refused legacy frame (protocol v27, `MIN_PROTOCOL` raised; SIM
  v20 pairs with it), the extract tab left the alchemy screen, and the bucket charge stopped being an Aqua source. A
  siphon draw is the currency's only faucet, which is what makes "no craft creates essence" structural rather than a
  per-level proof.
- **Seven node kinds, two reveal axes.** The four ore tiers keep Mining's reveals; the three SURFACE nodes reveal on
  ALCHEMY (`unlock::alchemy_reveal`), so Alchemy owns the surface hunt the way Mining owns the deep one: **Heartwood**
  in the trunks of worldgen trees (the day-one node — the economy must be able to begin, so its reveal is the level
  everyone starts at; landmark trees always carry a heart, trunk entire), **Leystone** riding the ore deposit stream's
  new shallow band in pristine Stone, and **Springstone** in the pristine sand beds of lakes and rivers — under water on
  purpose, since reaching it is the trip. The unified question set lives in `worldgen/nodes.rs`
  (`revealed`/`dig_mode`/`spent_block` over `NODES`), and the fleck palettes, spent faces and textures follow the ore
  tiers' pattern exactly.
- **The ore bands gained FLOORS.** §5.3's overlap divergence is re-decided: every tier now has a floor as well as a
  ceiling, adjacent bands overlapping only at their seams, with the first tier confined to surface-cave depths — so
  depth predicts tier and a find at the bottom of the world is never common ore again. The old "exclusive bands strand a
  low-level miner at depth" concern is answered by the reveal ladder itself: you mine where your tier lives.
- **Crafts are feedstock + catalyst.** A recipe consumes MATTER (one cube of the material's feedstock: timber from the
  felled log, masonry from Rubble, the sand-likes from Sand, the soils from themselves) plus a small essence CATALYST,
  and yields `shapes::craft_batch` pieces — as many as the shape's volume cuts from a cube, so matter conservation is
  structural. Metal and Crystal stay essence-only at a cube's worth: nothing in the world is made of them, and
  transmutation proper is what they are for. Bulk building therefore costs mostly the rock you quarried, and essence
  taxes lightly but universally — the purpose it lacked. Waste still applies, to the catalyst alone.
- **The deadlock rule generalized: the essence tool costs no essence, ever.** Every essence is a siphon draw now, so the
  siphon — and the bucket, which shares its day-one footing — is priced in feedstock alone (felled timber), pinned by
  `the_essence_tool_costs_no_essence`. "Dig your way to the trees, fell a few, make your vessels" is the opening loop;
  the heartwood you can already see is the first draw, and leystone at its early reveal supplies the torch.
- **XP follows the finds, and each award carries more.** Alchemy XP is field draws (`skills::field_xp`, now covering
  every node kind — the surface kinds pay Alchemy-primary with a Mining cut, inverting the ore tiers' split, because the
  headline follows the skill that revealed the find) plus craft XP, and nothing repeatable against an unbounded supply.
  Because the skill earns far less OFTEN than under the extraction drip, the award side rose to meet it: §13.4's
  craft-XP halving is deliberately un-pulled (`craft_xp` pays the full worth — the halving existed to pace an unbounded
  bench, and the bench is bounded now), the ore tiers' Alchemy cuts sit high against their Mining headlines, and the
  surface draws are sized so a handful of heartwood hearts reaches the leystone reveal inside the first session. The
  bonus lines died with the bench; their flavors are free to return as node tiers in later waves.
- **The reagent identities.** The five essences kept their frozen wire indices and took display names that read as
  reagents now that each one is a find: VITAE (heartwood), TERRA (leystone), UNDA (springstone), FERRUM (the ore tiers),
  AETHER unchanged. §12's "display name for Metal" question closes with this.
- **Captures**: `DIG_NODE=<heartwood|leystone|springstone>` is `DIG_ORE`'s surface twin — pose at the nearest node with
  its reveal granted; leystone reuses the generalized ore chamber, and the surface kinds deliberately write nothing.

### 14.2 What it supersedes, and what stands

Superseded: §3.1's extraction table and §3.2's efficiency-as-extraction reading (the curve survives as the DRAW's
scaling), §3.3's bonus lines, §4.1's extraction XP row, §4.2's extract-side closure arguments, §7.1's extract-then-craft
loop wording (the Rubble rule itself STANDS — Rubble is now the mason's feedstock, which is a sharper version of the
same idea), §7.5's XP half (`convert_xp` is gone with the verb it policed; the catalog gate on `is_bench_only` stands),
§8.1's EXTRACT tab, and §13.5's "dig-and-extract pays for the bucket" beat.

Standing, unchanged in spirit: the no-RNG constitution (the Duskstone trace is still derived, never rolled), the
material × shape grid as the content multiplier, threshold unlocks as the reward language, the reveal-as-event rule, the
payability pin (now asked over nodes and feedstock — `every_functional_price_has_a_source_at_its_own_level`), the
anti-dupe's edited-bit, and the background bench (which now paces only crafts).

### 14.3 Open ends this creates

- **Balance is unmeasured.** Node densities, draw yields, field-XP figures, catalyst sizes and the two vessels'
  feedstock prices are first-guess numbers in `nodes.rs`/`ore.rs`/`material.rs`/`alchemy.rs`/`skills.rs`; §13.4's
  instrument-before-retuning rule applies with fresh force, and the first bot-soak should log essence/hour per node kind
  alongside XP/hour.
- **Node tiers.** The surface kinds ship one tier each. Richer variants at later Alchemy reveals (the pattern the ore
  ladder proves out), an Alchemy prospector's sense, and a proper DELVE-DESIGN handshake — its governed Aether faucet is
  the underground twin of the surface nodes and should speak this vocabulary — are the obvious next waves.
- **The inventory grid gained its seventh row** for the spent-face trophies, and `theme::FIT_FLOOR` stepped once to hold
  the smallest windows; the scrolling grid remains the real fix on record.
- **The essence exchange** (§4.3's any → any, never into Aether) reads differently now that essence is scarce — it may
  deserve to arrive earlier than the endgame slot the old ladder gave it.
