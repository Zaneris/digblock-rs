# Skills, Essence & Ore — design document

Status: **§11's M1–M7 are BUILT and merged. The document below is the
design as written; where the implementation diverged, the code and
CLAUDE.md are the authority and the divergences are listed here.**

**Done:** M1 skills skeleton · M2 essence economy + the two-tab alchemy
screen · M3 level-scaled dig speed · M4 tool slot + Stage-A emissive +
Torch · the RTGI rewrite (GI-DESIGN.md replaced LIGHT-DESIGN.md's Stage-B
voxel lighting entirely) · **M5 ore** (`worldgen/ore.rs`, the reveal
stamp, the Essence Siphon, field extraction, dual XP) · **M6 the Resonant
Pick** + prospector's sense + the 99 mastery · **M7's first wave** (§7.2's
material × shape GRID, three timbers, furniture, dressed stone, glass,
metal, crystal, the two lamps, the sigil).

**Where the code diverges from this document, and why:**

- **§7.2's grid is the implementation, not a curation rule.**
  `unlock::craft` is DERIVED as `max(shape, material)` from
  `core/material.rs` and `block::ShapeFamily`; craft cost and extraction
  yield prorate the material's 27-unit split by the shape's volume. The
  BLOCKS shape sits at level 1, so a material arrives in every form you
  have already learned. Five thresholds moved (§4.3's table is stale
  where it disagrees): Wood 20 → 1, Dirt/Sand 20 → 5, StoneSlab 1 → 20,
  StoneStairs 5 → 20.
- **§5.3's depth bands OVERLAP.** Exclusive bands would leave a Mining-10
  player at y = 20 finding nothing at all; every tier stays available all
  the way down and the deep ones are simply rarer.
- **Ore ids are 48–52**, not the doc's 30–34 (the lumber set took those).
- **The reveal stamps block IDS into the mesh input**, not texture layers
  — the mesher, the emissive bit and `block_tint` then need no changes at
  all. The mining level reaches the worker in an `OreStamp` rather than
  as a mesher field.
- **Multiplayer keeps an `EditOverlay`** (only re-application is
  conditional), because the anti-dupe's "has this cell been written?"
  question is one the client has to answer too.
- **Field extraction needs no new wire**; only the tool refusal does
  (`RejectReason::NeedsTool`, protocol v21).
- **The pick is a PLANE, not a bore** — 3 wide at 55, 9 at 85 — and its
  timing multiplier is LEVEL-only, never derived from how many cells
  qualify, or the two ends disagree by 3× over a single differing cell.
- **Furniture and the bed are FUNCTIONAL items**, not grid cells: a table
  is a thing, not a form any substance takes. The bed is one cell (a
  horizontal two-cell pair is new machinery — the door's pair is
  vertical), and its Flora + Aqua price is the catalog's first genuinely
  cross-family cost.
- **Glass ships opaque** (§12's render question is unanswered: the depth
  prepass writes the whole quad, so a cutout discard needs the prepass to
  sample the atlas).
- **Ore's bench XP is `ceil(base/3)` like everything else** (6/12/20/33),
  not §4.1's hand-set 15/30/50/80 — the rule was kept, the exception
  dropped. Field XP (§5.4) is tabled as written.

**Still design:** §7.2's later SHAPES (fence, ladder, pillar, pane, gate,
trapdoor — each a `shapes.rs` project), the 3:1 essence exchange and the
Water craft's own `AlchemyOp` arm, the ore-draw effect's remote half (a
snapshot field for who is drawing on which cell) and its sustained audio
loop, the prospector's own sourced shimmer, and §12's open questions.

Milestone detail as originally built:
M1: `digblock-core/src/skills.rs` (curve + awards, golden-pinned), protocol v16
(`Skills`/`XpGain`), `inventories.bin` v4, `state.json` skills, both server chokepoints
awarding, XP toasts, the character screen (C / pause row / `DIG_SCREEN=skills`), offline
parity through the same core functions. M2: `digblock-core/src/alchemy.rs` (extraction /
waste / bonus tables, all integer math, `no_level_ever_creates_matter` pinned), the five
essences with **legacy-preserving indices** (Flora=0, Earth=1 — the entire "decode-time
migration" is that numbering choice, see §9), the Rubble rule, level-aware
`apply_alchemy`, the two-tab screen with the drag-to-slot drip
(`DIG_SCREEN=alchemy|transmute`), and the server extraction bucket
(`extraction_pace_is_server_enforced`). Two design bugs §3.3 had were caught in
implementation and fixed structurally — see the bonus-line pump rule there. M3 (level-
scaled dig speed, SIM v11, exact parity at 30) is landed, along with a rework of the
transmute tab into §8.1's family model. M4 is HALF landed: the Stage-A emissive bit
(solid vertex word 1 bit 21; chunk.wgsl returns fogged ink and skips sun/shadow/AO) and
the Torch (ShapeKind::Torch — post + glowing head, hand-priced 4 Flora + 2 Earth at
Alch 8, the LIGHTS family on the transmute tab, in the DIG_SHAPES showcase). M4's other
half — §6's tool formalization — is landed too: tools are item-only `blocks!` rows
(`Bucket = 29`, `is_tool`/`always_owned`), the tool slot is PINNED to the bar's end
(legacy layouts that parked it mid-bar migrate on decode), `Hotbar::active_tool` is the
second axis riding the Layout wire's 9th entry (`WIRE_TOOL` = the bucket), tap-9 on the
selected tool slot cycles the owned roster, the faded rail draws once a second tool
exists, and the server's Collect/pour gates check the active tool. A consolidation pass
(2026-08-10) then landed **Stage-B voxel lighting** (LIGHT-DESIGN.md — torches
illuminate, caves and roofed rooms go dark; §7.4 updated) and reconciled this document
with the shipped code (§3.3 cumulative note, §4.1/§4.2 per-line-ceil ideals, §4.3 rows
75/90, §5.3 ore ids 30–34, §10 pump-rule pin), then landed §5.1's **per-material dig
families and the rock gate** (SIM v12, protocol v17 — Fixed/Soft/Wood/Rock curves all
exact at 30; Sandstone opens at Mining 3, Stone/Rubble at 5), and its Alchemy twin, the
**craft gate** (SIM v13, protocol v19 — a craft-only block refuses to be dug below the
Alchemy level that makes it, so a pickup can't route around an unlock; the planed-timber
set moved 40 → **15** with it, joining the bucket charge). M5+ remain design. Originally fleshed out from the brainstorm of 2026-08-09, grounded in
the code as it stood (protocol v15, SIM v10, `inventories.bin` v3). Every number in here
is a tuning proposal, not a promise — but the *shapes* (which functions exist, where
authority lives, what migrates how) are meant to be the real plan.

---

## 1. Goals and non-negotiables

Two launch skills — **Mining** and **Alchemy** — levels 1–99, RuneScape-flavored: slow
exponential curve, threshold unlocks that feel like events, XP from doing the thing.

Design rules that bind everything below:

- **No RNG anywhere.** Yields, ore placement, XP are all pure functions. Offline local
  worlds run the identical rules through `apply_alchemy`/the dig path, and every economy
  invariant in this repo is pinned by deterministic tests. "Chance of bonus" mechanics are
  replaced by threshold unlocks, which are the better feel anyway.
- **XP is server-computed, never client-claimed.** Awards happen at the two existing
  server chokepoints (the dig-credit block in `edits.rs` ~535–560 and `process_alchemy`).
  The client learns its XP over the reliable stream and derives levels locally from the
  shared curve.
- **No matter creation.** Extraction efficiency ≤ 100%, transmutation waste ≥ 1.0×, and a
  block's total extraction base is strictly less than its craft cost at every level.
  The old exact-closure economy (3 slabs = 27 = one block) is deliberately retired —
  lossiness scaled by level *is* the progression — and replaced by monotonicity pins.
- **Ore is derived, never stored.** Chunk data, the journal, and the wire keep carrying
  Stone. No worldgen version bump, no journal migration, golden hashes untouched,
  existing worlds grow ore retroactively.
- **Reuse the count-based inventory for every new currency** (the water-charge precedent):
  essences are flagged-id counts, so prediction, `inv_delta` rollback, `InventoryFull`
  rebase, `player_store`, and `state.json` all carry them with zero new machinery.

---

## 2. The XP curve (shared by all skills)

XP to advance from level L to L+1:

```
xp_to_next(L) = floor(83 · 2^(L/10))        // doubles every 10 levels
```

Cumulative table (pinned as a golden constant when built):

| level | XP to next | cumulative |
|------:|-----------:|-----------:|
| 2 | 95 | 88 |
| 5 | 117 | 394 |
| 10 | 166 | 1,068 |
| 20 | 332 | 3,376 |
| 30 | 664 | 7,998 |
| 40 | 1,328 | 17,246 |
| 50 | 2,656 | 35,745 |
| 60 | 5,312 | 72,745 |
| 70 | 10,624 | 146,751 |
| 80 | 21,248 | 294,767 |
| 90 | 42,496 | 590,805 |
| 92 | 48,815 | 678,847 |
| 99 | — | **1,103,585** |

Properties: level 50 costs ~3% of the total; the last ten levels cost roughly half of it
(the RS "level 92 is halfway" feel, softened — here ~89 is halfway). At the endgame XP
rates below, 99 Mining is on the order of 50–70 hours of focused ore work; 99 Alchemy
rides along at maybe 60–70% of that if you process everything you mine. Tune by changing
the single `83` coefficient.

Storage is **XP only** (`u64` per skill); level is always derived. `Skill` is a `repr(u8)`
enum (`Mining = 0`, `Alchemy = 1`, append-only) so persistence and wire carry
`Vec<(u8, u64)>` and unknown skills from the future decode harmlessly.

---

## 3. Essences

Five, replacing the two `Material` sub-material families outright:

| essence | flavor | primary sources |
|---|---|---|
| **Earth** | mineral bulk | stone, sandstone, gravel, sand, dirt |
| **Flora** | living matter | wood, grass tops, leaves |
| **Aqua** | water | bucket charge, snow, mud traces |
| **Metal** | refined ore | the four ore blocks (bench-processed) |
| **Aether** | the rare one | Aetherock, high-level bonus lines only — **cannot be transmuted into, ever** |

Denominated in the existing 27ths-of-a-block base (`SUBS_PER_BLOCK = 27`,
`SUB_UNIT_VOLUME = 64` twelfths³ — both unchanged).

### 3.1 Extraction table (bench, base yields at 100% efficiency)

The full-roster table `sub_material()` grows into. Base totals are deliberately **≤ 27**
— the shortfall is "structure lost to the process" and is what makes extract→craft→extract
a sink at every level (see §4.2).

| block | Earth | Flora | Aqua | Metal | Aether | notes |
|---|--:|--:|--:|--:|--:|---|
| Stone | 27 | | | | | the pure baseline — craft-only in practice (§7.1); closes the recycle loop |
| **Rubble** | 24 | | | | | what mined rock actually is (§7.1); the mass Earth feedstock |
| Sandstone | 21 | | | | | craft-only in practice — mined sandstone is Rubble too |
| **Glass** | 21 | | | | | crafted blocks extract back at a loss, like everything |
| Gravel | 15 | | | | | |
| Sand | 12 | | | | | |
| Dirt | 15 | 3 | | | | |
| Grass | 18 | 9 | | | | the canonical 2/3–1/3 split |
| DryGrass | 18 | 6 | | | | |
| Mud | 12 | 3 | 3 | | | |
| Wood | | 27 | | | | |
| Leaves / PineLeaves | | 1 | | | | foliage is water and structure; the essence is in the trunk (was 3 — at a 10-tick dig that paid Flora as fast as grass) |
| Snow | | | 9 | | | cold biomes' economic identity |
| Water (bucket charge) | | | 18 | | | unlocked Alch 15; consumes the charge |
| WoodStairs | | 18 | | | | shaped items extract at volume — |
| StoneStairs | 18 | | | | | crafting is no longer one-way |
| WoodDoor | | 10 | | | | |
| WoodSlab / StoneSlab | (9) | (9) | | | | in their family |
| Emberore | | | | 18 | | |
| Duskstone | | | | 36 | | |
| Glowvein | | | | 60 | | |
| Aetherock | | | | 90 | 9 | the only base Aether source |

### 3.2 Level scaling

```
efficiency(L) = 40% + 60% · (L−1)/98          // 40% at 1 → 100% at 99
yield_per_line = max(1, floor(base · efficiency(L)))   // min 1 only where base ≥ 1
```

Level 1 grass: `floor(18·0.4)=7` Earth + `floor(9·0.4)=3` Flora. Level 99: the full 18+9.
A rich find hoarded until your Alchemy rises is worth more later — deliberate.

### 3.3 Bonus lines (the "rarer essence unlocks")

Flat additions, **not** scaled by efficiency — the unlock is the whole point.

**The pump rule (found in implementation, pinned by
`bonus_lines_never_sit_on_craftable_blocks`):** a bonus line on a *craftable* block is
an essence pump — craft it from its cost essences, extract it back, pocket the bonus.
At 99 the originally-drafted lines turned 2 Earth into 1 Metal (Stone trace minerals)
and, worse, 2 Flora into 1 Aether (Wood heartwood), violating "Aether cannot be
transmuted into, ever". So bonus lines live ONLY on recipe-less blocks: mined rock
(Rubble, Gravel) and foliage. The heartwood flavor moved to pine resin:

| Alch level | unlock |
|--:|---|
| 25 | Leaves and PineLeaves +1 Aqua ("morning dew") |
| 40 | **Rubble** +1 Metal ("trace minerals" — mined rock, never craftable Stone) |
| 60 | Gravel +2 Metal |
| 75 | **PineLeaves** +1 Aether ("resin" — pine forests are the pre-ore Aether trickle) |
| 90 | **Rubble** +1 Aether |

Lines are **cumulative**: each row adds to the ones before it, so a level-90
extraction of Rubble yields the 40 trace minerals AND the 90 Aether, and
PineLeaves at 75 keeps its 25 dew (`bonus_lines` returns every earned line).

---

## 4. Alchemy (the skill)

### 4.1 XP sources

| action | XP |
|---|---|
| bench extraction, per block | `ceil(base_total / 3)` — Stone/Wood/Grass 9, Dirt 6, Snow 3, bucket charge 6 |
| bench extraction, per ore | Emberore 15, Duskstone 30, Glowvein 50, Aetherock 80 |
| field ore extraction (the dual-XP cut, see §5.4) | 8 / 14 / 22 / 34 by tier |
| transmutation, per craft | `ceil(ideal_cost / 3)` — slab 3, door 4, stairs 6, raw block 9–10 (per-line ceil pushes Dirt/Mud ideals to 28 → XP 10; pinned by `alchemy_xp_derives_from_the_base_tables`), Resonant Pick 104 (312-unit ideal) |

XP keys on the *base* value, never the level-scaled yield — per-action XP is fixed for
life, RS-style. Extraction XP arrives **dripped**: the extract tab sends one
`Convert { count: 1 }` per completed cycle (§8.1), so a hoard of 999 stone is ~9k Alch
XP over ~5 minutes at the bench, not in a click — feeding the coalescing XP toast a
steady pulse. `ALCHEMY_MAX_COUNT = 999` stays in the protocol untouched (crafting
multipliers still batch, and the wire doesn't care who paces it).

### 4.2 Transmutation cost

```
waste(L) = 1.60 − 0.55 · (L−1)/98             // 1.60× at 1 → 1.05× at 99
cost_per_line = ceil(ideal · waste(L))
```

`ideal` stays the shipped volume rule — `raster_volume(...).div_ceil(SUB_UNIT_VOLUME)` —
so the pinned `craft_costs_derive_from_collision_volume` test keeps working with a `waste`
multiplier layered on top. Raw-block recipes cost the **per-line ceil of the 27-volume split** in the
block's extraction ratio (Dirt = 23 Earth + 5 Flora = a 28-unit ideal — each line
rounds up separately), which combined with §3.1's ≤ 27 extraction bases yields the
no-matter-creation pin with margin to spare.

### 4.3 Level table

| Alch | unlock |
|--:|---|
| 1 | extract the basic roster; **Slab** shape (ideal 9) |
| 5 | **Stairs** shape (18) |
| 8 | craft the **Torch** (4 Flora + 2 Earth — light before ore, §7.3) |
| 10 | craft the **Essence Siphon** (30 Flora + 15 Earth — pairs with Mining 10's first reveal). (The **Door** shape was authored here; it ships as part of the level-15 planed-timber set instead — see below.) |
| 15 | **extract the bucket charge** → 18 Aqua (the Aqua economy opens); the **planed-timber** material and its whole joinery set (Lumber block, slab, stairs, door — §7.2's `max(shape, material)` rule with the material dominating every shape) |
| 20 | materials: **clean Stone** (the mason's unlock, §7.1), Wood, Dirt, Sand |
| 25 | bonus line: dew (§3.3); extraction pace 0.5s → 0.35s per WHOLE block (every block's own pace scales with it); **Stone Bricks** |
| 30 | materials: **Sandstone, Snow, Glass, Grass, Mud** |
| 35 | **Fence**, **Ladder** shapes (§7.2) |
| 40 | bonus line: trace minerals; craft **Lantern**, **Framed Window** |
| 45 | **Pillar**, **Roof Wedge** shapes; materials: **Polished Stone**, **Sandstone Bricks** |
| 50 | **craft Water**: 27 Aqua → 1 bucket charge (lossy vs. the 18 extraction — no loop); material: **Metal** (block + its grid shapes: MetalStairs, MetalDoor, …) |
| 55 | craft the **Resonant Pick** (240 Metal + 60 Earth + 12 Aether) |
| 60 | **Pane**, **Gate** shapes (Glass Pane, Metal Bars); essence transmutation 3:1 (any → any, **except into Aether**) |
| 70 | **Trapdoor** shape |
| 75 | bonus line: pine resin (PineLeaves — §3.3 moved it off craftable Wood); extraction pace → 0.25s per whole block |
| 80 | craft the **Aetherlamp** (12 Metal + 3 Aether — the standing Aether sink) |
| 85 | waste floor reached early: 1.10× from here (flat to 99's 1.05×) |
| 90 | bonus line: Rubble Aether (Stone is craftable — a Stone line would be the §3.3 essence pump) |
| 99 | efficiency 100%, waste 1.05×; craft the **Master's Sigil** (99 Aether, cosmetic held item) |

Material and shape unlocks compose per §7.2's `max(shape, material)` rule — StoneDoor
lands at 20, not a curated 70.

---

## 5. Mining (the skill)

### 5.1 Breaking strength (SIM v12: per-material families + the rock gate)

**Today's speed is the mid-curve reference, not the floor — per FAMILY.** Every diggable
block carries a `DigFamily` (`blocks!`'s `dig` builder takes it beside hardness, so a
row can't forget it), and every family's curve passes EXACTLY through 1000‰ at level 30
— the legacy hardness table is the shared mid-curve golden — so a family only shapes the
approach and the ceiling:

| family | members | L1 | L30 | L99 | why |
|---|---|--:|--:|--:|---|
| **Fixed** | Leaves, PineLeaves, Torch, Water (scoop) | 1000 | 1000 | 1000 | already near-instant / fixtures; the scoop is pinned to the viewmodel swing |
| **Soft** | Dirt, Grass, DryGrass, Sand, Mud, Snow | 1250 | 1000 | 417 | the original gentle start |
| **Wood** | Wood + its shapes | 1350 | 1000 | 500 | timber resists a little more, masters a little less |
| **Rock** | Stone, Sandstone, Gravel, Rubble + stone shapes | 1700 | 1000 | 417 | rock resists a novice viscerally |

```
permille(family, L): two exact-anchored round-half-up segments
  L ≤ 30: start − (2·(start−1000)·(L−1) + 29) / 58
  L > 30: 1000 − (2·(1000−end)·(L−30) + 69) / 138
required = max(1, round(hardness_ticks · permille / 1000))   // skills::dig_required(block, h, L)
```

The level-99 endpoint IS the speed floor — no second clamp knob, and the flat Fixed
family is what keeps leaves/torches/the scoop from ever speeding up. Spot values: Stone
2.55s at L1 (153t) → 1.50s at 30 → 0.63s at 99; Wood 1.35s → 1.00s → 0.50s; Dirt 0.63s
→ 0.50s → 0.21s.

**Letting go decays, it does not reset** (`skills::dig_decayed`, `DIG_DECAY_TICKS`).
A released hold keeps its banked ticks and bleeds them back to untouched at **one whole
bar per second** — so one second is the *ceiling*, paid only by a block a tick from
breaking, and a barely-started hold is gone almost at once. A hold resumed inside the
window inherits what is left, and the crack reads the same number, so it visibly
retreats and picks back up rather than snapping between full and gone. Setting the rate
as a fraction of the bar rather than in ticks is what makes the web retreat at one
visible speed on leaves and on stone alike.

Both ends run that one function over their own release→resume interval. On a stable
link those intervals are the SAME length — the client's release and its restart are
each shifted by the same one-way delay — so only the jitter between the two separates
them, which is what `dig_grace` already absorbs. The server also errs the safe way at
every seam: `DIG_OFF_TARGET_RESET` starts its decay half a second *after* the client
gave the cell up, and `verify_digs` runs ahead of the action queue, so its banked
number is never below the one the player's bar is showing. Progress is stored as the
banked PEAK plus an idle count, never decremented in place, so the two sides compute
one number from the same two integers instead of two rounding chains.

**The rock gate** (`unlock::mine`) is a deliberate reversal of this section's
original "no strength wall" stance: Sandstone opens at Mining **3**, Stone (+ its
shapes) and Rubble at **5**; Gravel stays the ungated entry rock, so a fresh player
levels on soil and scree (~80 dirt digs to 5) before rock opens. Refusal happens at
`DigStart` via `RejectReason::SkillGated` (protocol v17) — the `Protected` pattern, so
nobody holds a button through a dig timer for nothing — and the client predicts it in
`diggable()` (it knows its own level; `unlock::Kind::Strength`'s message is the shared wording). Ore
(§5.3) stays gated by *visibility* on top of this.

**The craft gate** (`unlock::pickup`, SIM v13 / protocol v19) is the same
refusal on the other skill: a **craft-only** block — one worldgen never places, marked
`crafted` in its `blocks!` row and pinned against worldgen itself by
`worldgen_never_places_a_crafted_block` — refuses to be dug below the Alchemy level of
its own §4 recipe. Every one standing in a world was made by somebody, so picking one up
is a way of *having* it, and mining must not route around an unlock: WoodStairs/
StoneStairs at Alch **5**, Torch at **8**, the whole planed-timber set at **15** (slabs
sit at recipe level 1, i.e. ungated in practice). The gate keys on the DROP, not the
cell, which is what keeps pristine Stone a Mining question — it hands out Rubble, and
Rubble has no recipe at all — and on craft-*only* rather than craftable, or the level-20
`Wood` recipe would shut a fresh player out of the first tree they see. Same machinery
as the rock gate: refused at `DigStart` with `RejectReason::NotUnlocked`, predicted in
`diggable()`, one shared wording in `unlock::Kind::Recipe`'s message ("you haven't unlocked this
yet"). Placing, opening and walking on other people's builds are all untouched — only
the pickup is refused.

The server's completion gate already carries `dig_grace(need) = 4 + min(need/5, 12)`; a
level-up landing mid-hold can skew client/server `required` by a few ticks and the grace
absorbs it — no new tolerance machinery. Bedrock `y ≤ 0` stays undiggable, as ever.

### 5.2 XP per dig

`ceil(hardness_ticks / 6)`, awarded in the existing dig-credit block:

Leaves 2 · Snow 3 · Sand/Mud/Dirt/Grass 5 · Gravel 8 · Wood 10 · Sandstone 12 · Stone 15
· shaped blocks by their own ticks (a slab third pays 4). Ore field-extraction pays
tier XP instead: **40 / 70 / 110 / 170** Mining (+ the Alchemy cut, §5.4).

### 5.3 Ore

Four ores + a depletion block = **five new `blocks!` rows** (append-only, one PNG +
`LAYERS` entry each; names are placeholders until they survive contact with you):

| id | block | reveal at | depth band | hardness | bench yield |
|--|---|--:|---|--:|---|
| 30 | **Emberore** | Mining 10 | y ≤ 140 | 120t | 18 Metal |
| 31 | **Duskstone** | Mining 30 | y ≤ 96 | 150t | 36 Metal |
| 32 | **Glowvein** | Mining 55 | y ≤ 56 | 190t | 60 Metal |
| 33 | **Aetherock** | Mining 80 | y ≤ 24 | 240t | 90 Metal + 9 Aether |
| 34 | **DepletedStone** | — | — | 90t | `drops: Some(Rubble)` — the tapped-rock face (§7.1) |

(Ids start at 30: the append-only `blocks!` table has since taken 27 = Rubble,
28 = Torch, 29 = Bucket.)

**Placement** — `ore_at(x, y, z, seed) → Option<OreKind>`, pure, in `worldgen/`:
per-family salted position hashes seed deposit centers (the `structures_near` pattern, so
pockets span chunk borders statelessly); a pocket is the 3–8 cells within its blob mask.
An ore manifests only where the **pristine** cell is Stone **and the cell is unedited**
(client: the `EditOverlay`/pipeline knows; server: the compacted journal's per-cell LWW
map answers "has this cell ever been written"). The unedited rule is the anti-dupe:
placing stone into a deposit coordinate can never regenerate the vein. Deeper = richer
tiers, so the level gate and the dig-deeper incentive reinforce each other.

**Reveal** — the mesher stamps ore texture layers over qualifying Stone cells when
`mining_level ≥ reveal`; the pipeline injects the level into the padded mesh snapshot the
same way `EditOverlay` already intervenes. Crossing a reveal threshold bumps generation
counters on resident chunks — a remesh sweep, sliced through the existing stream budget.
Veins *fade in around you* on the level-up. Below the gate the client renders, raycasts,
and digs plain Stone, and the server agrees entirely — a low-level player digging a
would-be vein cell gets stone and spends the vein. You dig what you can see.

**Extraction at the rock face** — requires the **Essence Siphon** (Alch 10, §7.3) as the
active tool. With it held, a dig-hold on a revealed ore cell is an extraction: on
completion both sides derive "this is an ore extraction" from `ore_at` + the digger's
level + the active tool (the `Layout` 9th entry the server already holds, in wire order
like every slot check) and write **DepletedStone** instead of Air, crediting the raw ore
item. `EditRequest{kind: Dig}` unchanged — no new wire. Without the siphon — bare hand
or pick alike — a revealed ore cell **refuses the dig at `DigStart`** via the existing
`ServerMsg::DigRefused` + `refused_cell` machinery, with its own message: if you can see
the vein, the game won't let you stone-dig it away by accident. Below the reveal gate
nothing changes — stone before, stone after, vein spent — because that player's client
and server agree there is nothing there to protect.

Consequences, all free: mining ore never excavates (the mountainside keeps its shape);
below-gate players see stone before and after; the depleted face gives the miner visible
feedback; and the write being a *different id* means the edit is unambiguously real to
the journal, LWW replay, and mesh invalidation (no same-value-write no-op risk).

**The draw** — extraction shows no crack decal (`dig_progress` reports `None`, the
water-tool precedent); the feedback is the pull. Through the hold, the ore's emissive
seams (§7.4's Stage-A bit, driven by a progress value instead of a constant) brighten
from their resting glitter to full bloom, while a stream of motes in the ore's color
detaches from the face and arcs into the held siphon — a small world-space emitter in
the `spray.rs` mold, so remote players watching a miner work see the stream too.
Completion pops a brief burst and leaves the DepletedStone face; a canceled hold lets
the seams decay back. The siphon's viewmodel is the other half of the effect — the
draw-in point the motes converge on.
Raw ore is an ordinary slottable, placeable item — a *placed* ore block is genuine chunk
data visible to everyone at any level and diggable at face value (dupe-safe round trip);
showcase your finds.

### 5.4 Dual XP

Field extraction is mechanically a mine, thematically a crude alchemical separation:
full Mining XP + a minor Alchemy cut at the rock face (8/14/22/34), with the substantial
Alchemy XP paid later at the bench (15/30/50/80). Each ore carries a fixed total split
across two moments; neither skill can be leveled entirely from inside the other.

### 5.5 Level table

| Mining | unlock |
|--:|---|
| 1 | dig speeds by family: soil 1.25×, wood 1.35×, rock 1.7× today's times (§5.1 — the climb back is the early game); rock itself is gated |
| 3 | **Sandstone opens** (the rock gate, §5.1) |
| 5 | **Stone and Rubble open** (+ their shapes) |
| every level | walks its family's curve toward **parity with today at 30** (§5.1) |
| 10 | **Emberore** reveals — extraction needs the Essence Siphon (Alch 10, §5.3), the first cross-skill handshake |
| 30 | **Duskstone** reveals |
| 55 | **Glowvein** reveals; the **Resonant Pick** becomes usable |
| 80 | **Aetherock** reveals |
| 85 | pick resonance deepens: 2 → 3 cells |
| 90 | **Prospector's sense**: a faint audio shimmer within 8 blocks of an unrevealed-tier deposit (a cadence emitter, like footsteps — audio only, no ESP overlay) |
| 99 | mastery: every field extraction yields +1 raw ore |

### 5.6 The Resonant Pick

Item-only `blocks!` row (like all tools, §6). Crafted Alch 55, usable Mining 55 — the
flagship cross-skill dependency. Held, a dig-hold on any diggable cell also digs the cell
**directly behind it along −normal, if it holds the same block id**; `required` is 1.5×
the single-cell time; both drops and both XP awards land. The pair is derived from the
server's own re-raycast (cell + face normal), never client-chosen — no cheat surface.
Writes ride `begin_multi` under one seq exactly like door pairs. At Mining 85 the reach
extends to 3 cells. Protection checks run per-cell via the assembled `writes` list, as
they already do.

---

## 6. Tools and switching

The current pinned-slot-8 WaterTool is not dismantled — it is **formalized**. The hotbar
becomes 8 block slots plus a dedicated **tool slot**, drawn visually separated from the
block row, with digit 9 / scroll reaching it exactly as today. Which tool sits *in* the
slot is a separate axis:

```
 [1][2][3][4][5][6][7][8]   ┃ [9]┃ ▫ ▫
  blocks (digits 1–8)         tool   faded rail: other owned tools
```

- **Ownership is inventory counts.** Tools are item-only `blocks!` rows —
  `RenderClass::None`, `collides: false`, hardness 0, not placeable, and
  **`is_slottable: false`** like water. Crafting the pick is `inventory.add(pick, 1)`;
  the tool rail lists every nonzero tool id; `reconcile_layout` keeps tools out of the
  block slots and the grid with the rule it already has. Ownership, prediction, rebase,
  and both persistence formats all ride the existing count machinery untouched. Launch
  roster: the **bucket** (owned from the start, as today), the **Essence Siphon**
  (crafted Alch 10), the **Resonant Pick** (crafted Alch 55) — three tools, which is
  exactly when tap-9 cycling and the rail earn their keep.
- **Cycling**: tapping **9 again while the tool slot is already selected** advances the
  active tool through the owned set, wrapping. No new keys, and touch gets the identical
  rule for free — first tap of the tool slot selects it, further taps cycle. Selecting the
  slot by scroll then tapping 9 also cycles (the rule is "9 on an already-active tool
  slot", nothing subtler). With one tool owned the extra taps no-op; the rail only draws
  when a second tool exists. Backward cycling is deliberately omitted — the roster is
  small and it wraps; Shift+9 can grow that meaning later without disturbing anything.
- **The faded rail**: small icons at reduced alpha beside the tool slot, active tool
  excluded, in a fixed owned-order (block-id order — deterministic, no arrangement state
  to persist or sync). It's a readout, not a target: clicking is not required, cycling is
  the interaction. Built as faded *slots* rather than bare icons — they sit over open
  world, and a dark tool on dark terrain was a smudge.
- **The swap is a handover, not a cut** (built after the fact, and it is what makes
  cycling read as one row rearranging itself rather than an icon glitching): the outgoing
  tool shrinks away and the incoming one grows in, through the same curve the transmute
  tab's family tiles rotate on (`ui::handover`). Everything else about the exchange —
  which tools the rail lists — happens at the midpoint, where every icon is at nothing,
  so no tool is ever in the slot and the rail at once. `ToolSwap` watches the active tool
  rather than being told about the keypress, so a server `Layout` rebase and a rejoin
  animate the same way a tap does.
- **Wire**: `Layout.hotbar` keeps its 9 entries and its format; the 9th entry's meaning
  becomes "the active tool id" (the sentinel `WIRE_TOOL` decodes as the migrated bucket
  id). Slot enforcement is unchanged — `EditRequest.slot = 8` + the server's copy of the
  active tool validates Place/Collect exactly as `WrongSlot` does today. Cycling is a
  `Layout` send, which already precedes edits on the reliable stream, so a
  cycle-then-scoop in one tick validates in wire order. `HELD_TOOL_FULL` stays as the
  remote-display sentinel for the charged bucket.
- Tool *use* gates (Mining 55 for the pick) are checked server-side at dig validation and
  mirrored client-side in `diggable` — an unusable owned tool still cycles into the slot
  but digs like a bare hand, so there's no hidden-tool edge case in the rail.

---

## 7. Expanded transmutation catalog

### 7.1 Rubble — why transmutation *is* the economy

Digging natural rock never yields the clean block. Stone's `blocks!` row gains
`drops: Some(Rubble)` — the same one-word machinery DepletedStone already uses — so every
mined stone lands in the inventory as **Rubble** (new id, append-only): a cracked, ugly,
fully placeable block. Clean Stone exists only two ways: pristine in the terrain, or
transmuted at the bench. The loop this creates is the point — a builder who wants clean
walls *needs* Alchemy, not just a miner's patience:

```
mine Stone → Rubble (24 Earth base)  →  extract  →  craft clean Stone (27-vol × waste)
```

- **Rubble is not junk.** Placed, it reads as rough cobble/drystone — the free rustic
  building material. Low-level builds are cobble builds; that's aesthetic progression.
- **Sandstone drops Rubble too** ("all broken rock"), so clean Sandstone is likewise
  craft-only (Alch 30). Sand, Gravel, Dirt are already loose material and drop themselves.
- **DepletedStone's drop changes** from `Some(Stone)` to `Some(Rubble)` — a tapped-out
  vein face crumbles like any other rock.
- **No hybrid recipes.** Refining is extract-then-craft through the existing
  `AlchemyOp::Convert`/`Craft` shapes — there is no "1 Rubble + N Earth → Stone" op,
  which would be new machinery for no gain. The Rubble→Stone exchange rate falls out of
  efficiency × waste: ~5 rubble per clean stone at level 1 (9 Earth out, 44 in), ~1.2
  at 99 (24 out, 29 in).
- Crafted/pristine clean Stone still extracts at 27 (§3.1), so the recycle loop closes
  and stays a sink: 27 back at 99 efficiency vs. `ceil(27·1.05) = 29` to craft.

### 7.2 The material × shape grid

Per-item curation doesn't scale to "metal stairs, stone doors, brick slabs, …". Two
unlock axes replace it for structural pieces:

**Materials** — can you transmute this substance into block form at all:

| Alch | material (craft cost per §4.2: 27-vol in extraction ratio) |
|--:|---|
| 15 | **Planed Timber** (`Lumber`) — shipped; the joinery material, so its slab/stairs/door all land here rather than at their shapes' own 1/5/10 |
| 20 | **Stone** (the mason's unlock, §7.1), Wood, Dirt, Sand |
| 25 | **Stone Bricks** — same cost as Stone, squared-off dressed texture |
| 30 | Sandstone, Snow, **Glass** (sand-flavored Earth), **Grass**, **Mud** |
| 45 | **Polished Stone**, **Sandstone Bricks** |
| 50 | **Metal** — 27-vol Metal essence; scarce by construction, the prestige block |

**Shapes** — can you work any substance into this form:

| Alch | shape | ideal (volume rule) |
|--:|---|---|
| 1 | Slab | 9 |
| 5 | Stairs | 18 |
| 10 | Door | 10 |
| 35 | Fence (posts + two rails), Ladder (climbable = §12) | ~5 / ~3 |
| 45 | Pillar (6×12×6), Roof Wedge (45° half-prism, ramp physics) | ~7 / ~14 |
| 60 | Pane (12×12×1), Gate (fence-height door) | ~3 / ~6 |
| 70 | Trapdoor (horizontal door, new facing axis) | ~5 |

A shaped item in material M gates at **`max(shape level, material level)`**: StoneDoor at
20 (door 10, Stone 20 — replaces the old curated 70), MetalStairs at 50, Metal Bars
(pane in Metal) at 60, Brick Slab at 25, Glass Pane at 60. Each authored pair is still
its own `blocks!` row + PNG + baked template, append-only — the grid is the *gating
rule*, not a promise that every cell exists (nobody is owed Snow stairs). Costs stay
volume-derived, so the pinned volume test covers every pair for free, and multi-essence
materials (Grass, Mud) split shaped costs in the same extraction ratio §4.2 already
defines for raw blocks.

### 7.3 Functional items (authored costs)

Matter is priced by volume; **function is priced by hand** — the Resonant Pick set the
precedent (a pick's cost is obviously not its volume). Multi-essence costs are deliberate
here: cross-family recipes make the essences feel like an economy, not parallel currencies.

| item | ideal cost | Alch | notes |
|---|---|--:|---|
| **Torch** | 4 Flora + 2 Earth | 8 | first light, pre-ore on purpose; emissive (§7.4) |
| **Essence Siphon** | 30 Flora + 15 Earth | 10 | the ore-extraction tool (§5.3); **no Metal in the cost, ever** — ore needs the siphon and Metal needs ore, so a Metal-priced siphon deadlocks the economy |
| **Framed Window** | 3 Earth + 2 Flora | 40 | glass-in-wood-frame, the early window |
| **Lantern** | 8 Metal + 6 Earth | 40 | metal-and-glass; first Metal sink outside tools |
| **Water** (bucket charge) | 27 Aqua | 50 | lossy vs. the 18 extraction — no loop |
| **Resonant Pick** | 240 M + 60 E + 12 Ae | 55 | tool (§5.6) |
| essence exchange 3:1 | any → any **except Aether** | 60 | |
| **Aetherlamp** | 12 Metal + 3 Aether | 80 | brightest light; the standing Aether sink |
| **Master's Sigil** | 99 Aether | 99 | cosmetic held item |

The Aetherlamp matters economically: before it, Aether's only sinks are the pick (12,
once) and the Sigil (99, once). A repeatable 3-per-lamp sink is what keeps Aetherock
worth mining at 90+.

**Tools cap at one** — `inventory::cap()` returns 1 for every tool id, the exact
water-charge precedent. A second craft refuses before any debit (the standing
`InventoryFull` rebase answer), and the transmute row for an owned tool draws dim with
**OWNED** where its button was — a trophy line, not a vanished entry, reusing the
locked-row pattern (§8.1). Same rule covers the Sigil. The alternative — granting the
pick automatically at a Mining level — was considered and rejected: it deletes the
Alch-55-craft/Mining-55-use cross-skill dependency and the game's biggest essence sink,
and it breaks "ownership is inventory counts" by needing a new grant-on-level-up path
with its own rejoin and migration questions.

### 7.4 Light sources and the lighting question

The engine has sun + shadow cascades and no point lights, so light ships in two honest
stages:

- **Stage A (ships with the catalog): emissive materials.** A flag on texture layers
  (`LAYERS`-adjacent, one material bit in the packed vertex) that makes the shader skip
  the sun/shadow term — fullbright ink. Torch flames, lantern glass, and the Aetherlamp
  glow in the dark; Glowvein and Aetherock ore seams get the same bit, so revealed veins
  glitter in dark caves (a free Mining synergy). Cheap: no new passes, no light field.
  Limitation stated plainly: a Stage-A torch is visible in the dark but does not
  illuminate its surroundings — it's a way-marker, not a lamp.
- **Stage B — LANDED (see LIGHT-DESIGN.md): true voxel light.** Torches cast a real
  flood-fill blocklight field and a full skylight column channel darkens caves and
  roofed rooms. One correction to this section's original sketch: propagation is NOT
  "via the padded shell" (a level-15 light reaches 15 cells, the shell reaches 1) — the
  field is stored per chunk beside the store and the shell only carries settled values
  into mesh jobs, the same role it plays for AO. `BlockProps.light`/`emits(level)` is
  the source knob (Torch 14; Lantern and the Aetherlamp slot in as rows), independent
  of `emissive` — ore seams still get ink that casts nothing.

Torches are floor-mounted in v1 (one shape, no facing states); wall mounting is a state
nibble away when wanted (§12).

Still deliberately out of v1: container blocks (chests — the inventory is already
generous) and mechanisms.

---

## 8. UI

### 8.1 Alchemy screen (total redo — SHIPPED; the mock below is the as-built layout)

The old single linear list died. Structure — same `Row`-stream architecture, theme
tokens, one glyph size, `fit_scale` ladder, everything on ONE left column, and the
panel height is the max of both tabs so the bottom-anchored top edge never jumps on a
tab switch (`panel_height_is_the_same_on_both_tabs`):

```
┌──────────────────────────────────────────────────────────┐
│ ALCHEMY   LV 41                     [██████████░░░░░░]   │  header: level + XP bar
│ FLORA 356   EARTH 1,204   AQUA 40   METAL 88   AETHER 2  │  essences (wire order, always visible)
│ ┌─────────┐                                              │
│ │ EXTRACT │  TRANSMUTE                                   │  real tabs: active fill fuses into
│─┘         └──────────────────────────────────────────────│  the page; the rule breaks under it
│ ┌────┐                                                   │
│ │⧉ 214│  → 24 EARTH   ·   1 block / 0.35s      [🪣 tile] │  extraction slot (count badge +
│ └[██░]┘                                                  │  progress inset) · preview, elided
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦                                       │  before the bucket tile
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦   the real inventory grid             │
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦   (same widget, drag sources)         │
│  ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦ ▦                                       │
└──────────────────────────────────────────────────────────┘
```

(TRANSMUTE swaps the strip+grid for: family tiles → member tiles — both wrapped at the
grid's 9 columns, locked tiles dimmed with a top-left unlock badge — then the
`ACTION_ROW_H` detail strip with the ONE TRANSMUTE button. Hover washes every
interactive tile; the selected family wears the WEAK ring, the acted-on member and an
armed extraction slot the STRONG one — two weights, one containment hierarchy.)

The EXTRACT tab is **your inventory plus one drop target** — the player picks the stack,
never the screen. Rules, each load-bearing:

- **Source is the real inventory grid** (and hotbar), the same widget, slot rendering,
  and `DragState` the inventory screen already uses. Drag a stack onto the extraction
  slot to begin; the yield preview and per-block pace draw beside it, at your level.
  The pace is the BLOCK's, not just the level's: `extract_pace_ticks(block, level)`
  scales the level's whole-block interval by the block's `convert_xp` against the 9-XP
  reference, floored so 1-XP foliage doesn't become a wire-op stream. Keying it on the
  XP table rather than a second table is what stops pace and pay disagreeing about
  which blocks are the substantial ones.
- **The extraction slot is a pointer, not a container.** It holds a selected block id;
  the stack never leaves the inventory, so the slot touches neither `reconcile_layout`
  nor the `Layout` wire — it is screen-local UI state, cleared when the screen closes.
  The grid cell visibly drains in place as the drip consumes it.
- **The drip is client-paced ordinary ops.** While the slot is occupied, a progress bar
  fills over the per-block period; each completion sends a normal
  `AlchemyOp::Convert { count: 1 }` down the action queue, the server answers with the
  standing `InventoryFull` rebase, XP drips per §4.1. **No new wire, no server-side
  machine state** — the server never learns the slot exists, and offline worlds run the
  identical rules through `apply_alchemy`. The base pace (see below) sits far under the
  shared edit token bucket (`EDIT_RATE_PER_SEC = 10`), so extraction never starves edits.
- **But the pace itself is server-enforced** — client pacing is presentation, not trust
  (the return-to-spawn precedent: the server holds the clock). `Convert` ops draw from a
  per-player **extraction bucket** alongside the general edit bucket. The bucket banks
  TICKS — one per tick, capacity two WHOLE blocks' worth (`extract_burst_ticks`) — and a
  `Convert` spends the requested block's own `extract_pace_ticks(block, level) × count`,
  so neither the legacy bulk-Convert path nor a stream of cheap blocks can sidestep the
  drip. An op arriving short refuses cleanly through the standing `InventoryFull`
  rebase: nothing debited, no XP, and the honest client self-heals for free (the refused
  block is still in the count; the next cycle re-sends). Burst 2 absorbs network
  bunching exactly as `EDIT_BURST` does. Unlike the spawn cooldown, nothing persists —
  the window is sub-second, so rejoin-to-reset buys a quarter second and isn't worth a
  `PlayerRecord` field. The gate lives in server op validation like the edit bucket
  does, NOT in `apply_alchemy` — pacing is anti-cheat, not an economy rule, and offline
  worlds keep running the identical economy.
- **Pace**: 1 block / 0.5s at Alch 1 → 0.35s at 25 → 0.25s at 75 (these *replace* the
  old batch-cap unlocks). A hoard of 999 stone is now ~5 minutes at the bench instead of
  a click — the RS-style "stand at the bench and watch the XP drip" is deliberate.
- **Stopping**: drag the stack out, click the slot, close the screen, or run the count
  to zero — the in-flight cycle simply never sends its op. Ops are atomic per block, so
  there is no partial debit to unwind, ever.
- **The bucket charge appears as a single water block** beside the grid whenever the
  tool is charged (it can't live in the grid — `is_slottable: false`), and extracting it
  **empties the tool**: the charge *is* the inventory's Water count of 1, so the ordinary
  `Convert` debit is the emptying — no special case anywhere, and the held bucket's
  geometry (`HELD_TOOL_FULL` → empty) follows the count like it already does after a
  pour. One drag, one cycle, 18 Aqua, tile gone.
- Extraction runs **only while the screen is open**. No AFK-extraction-while-mining; you
  stand at the bench.

TRANSMUTE stays click-based (crafting is a decision, not a grind), structured as
**families with one button** (reworked to this shape during the build, on feedback that
the flat row list didn't read): a row of FAMILY tiles — SLABS, STAIRS, DOORS, BLOCKS,
WATER — each slowly rotating its icon through its members (wood stairs, stone stairs, …)
on a shared clock; selecting a family lays its member tiles out beneath (locked members
visible but dim, affordable ones badged with their batch count); a detail strip shows
the selected member's name and post-waste cost at your level; and the tab has exactly
ONE `TRANSMUTE` button, disabled (no hit rect) for a locked or unaffordable pick.
`CLICK 1 · SHIFT 10 · CTRL 100` multipliers on that button. This is also the §7.2 grid's
natural UI: a new material × shape pair is one more member in an existing family, so the
catalog grows without the screen growing. Tools/essence-exchange rows join as families
when they land (M4+). `fit_scale` remains the overflow fallback.

### 8.2 Character screen

New screen, `C` key (new `Key` variant) + a pause-menu row + `DIG_SCREEN=skills`:

```
┌────────────────────────────────────┐
│ CHARACTER                          │
│ MINING    Lv 47  [████████░░] 61%  │
│           next: Lv 55 — Glowvein   │
│ ALCHEMY   Lv 41  [██████░░░░] 44%  │
│           next: Lv 45 — Pillar     │
│ TOTAL     88                       │
└────────────────────────────────────┘
```

One row per skill: level, XP bar to next, next-unlock teaser — SHIPPED, with the
teaser prose screen-local and every named level pinned to the core function that
enforces it (`teaser_levels_match_the_core_tables`). Mining's teasers are the §5.1
rock-gate rows until ore lands (past Mining 5 the line simply drops). Clicking a row
could later expand the full unlock list; v1 is the summary.

### 8.3 Feedback

- **XP toasts**: a small accumulating `+45 MINING` above the hotbar, HUD-layer text over a
  `FILL_BG` backdrop (sized from `line_h`), coalescing rapid awards, fading after ~2s.
- **Level-up**: chat-feed line ("Mining is now level 48"), a one-shot sfx, and on reveal
  thresholds the remesh sweep plus a distinct line ("new veins shimmer into view").
- Both new screens get `DIG_SCREEN` stocking (essences + ores + XP included) so
  `--screenshot` can photograph them — the two shipped look-and-feel regressions both came
  from screens invisible to headless capture.

---

## 9. Wire, persistence, migration

**Protocol v15 → v16** (append-only as always):

- `ServerMsg::Skills { xp: Vec<(u8, u64)> }` — full sync, sent at join after `Welcome`.
- `ServerMsg::XpGain { skill: u8, total: u64 }` — new *total* (idempotent, reorder-safe),
  sent on award; the client animates the delta itself.
- No new `ClientMsg`. `AlchemyOp::Convert/Craft` shapes unchanged — the server-side tables
  behind them change meaning. `EditRequest` unchanged (ore extraction is derived, §5.3).
- Essence ids: the `Material` enum becomes `Essence { Flora=0, Earth=1, Aqua=2, Metal=3,
  Aether=4 }` riding the same `SUB_WIRE_FLAG | index` pairs. **The migration is a
  numbering choice, not code**: Flora keeps sub-wood's index 0 and Earth keeps
  sub-stone's 1, so every record and save written under the two-family economy decodes
  1:1 with zero migration paths anywhere (flagged pairs carry no version, so
  index-compat is the only migration that can't misfire). The water-stack precedent
  holds: a legacy count decodes to the new currency, never a windfall, never wiped.
- `SIM_VERSION 10 → 11`: level-scaled dig timing, ore hardness rows, DepletedStone.

**Persistence**:

- `inventories.bin` v3 → **v4**: `PlayerRecord` gains `skills: Vec<(u8, u64)>`; v1–v3
  read-compat (skills default empty = level 1). Existing players keep their inventories
  and start at level 1.
- `state.json`: a `skills` field with `#[serde(default)]` — same policy.
- The journal is **untouched** in format and version; DepletedStone/placed-ore cells are
  ordinary edits.

**Server hooks** — both are single existing chokepoints:

- Dig credit (`edits.rs` ~535–560): award Mining XP, run the ore derivation (write
  DepletedStone + credit ore + dual XP), then the existing `reconcile_layout` + persist.
- `process_alchemy`: award Alchemy XP alongside the existing `InventoryFull` answer;
  append the `XpGain` frame on the same reliable stream (in-order with the rebase).

---

## 10. Test plan (the pins that make this safe)

- `xp_curve_table_is_pinned` — golden cumulative table.
- `no_level_ever_creates_matter` — for every block × every level: total extraction yield
  < total craft cost; efficiency/waste monotonic and bounded.
- `bonus_lines_never_sit_on_craftable_blocks` — the §3.3 pump rule: every bonus line
  lives on a recipe-less block, so no craft-extract loop manufactures Metal or Aether.
- `dig_speed_anchors_are_pinned` — every family's level 30 reproduces the legacy
  hardness table **exactly** (the old table lives on as the shared mid-curve golden),
  the per-family 1/99 endpoints hold, Soft reproduces the SIM-v11 curve value for
  value, Fixed never scales, and `required` is monotonic non-increasing throughout.
- `dig_families_are_pinned` (explicit block → family roster, covering every diggable
  block) and `rock_is_gated_until_its_level` + the server half
  (`gated_rock_refuses_the_dig_start` — refusal at DigStart, no hold opened).
- `ore_positions_are_pinned` (golden per-seed, like `spawn_positions_are_pinned`) +
  density-histogram bounds per tier/depth band + `ore_never_manifests_in_edited_cells`.
- `field_extraction_writes_depleted_never_air` and the dupe test: place Stone into a
  pristine deposit cell → no ore.
- `mined_rock_is_always_rubble` — every rock-family `drops` points at Rubble, and the
  rubble→stone loop is a sink at every level (extract 24 < craft `ceil(27·waste)`).
- `grid_gates_compose` — every authored material × shape pair unlocks at exactly
  `max(shape level, material level)`; no curated stragglers.
- `extraction_pace_is_server_enforced` — Convert ops beyond `pace(block, level)` refuse
  with no debit and no XP; sustained throughput of a flooding client ≤ honest throughput
  at the same level; `count > 1` charges the bucket per block, not per op.
- Reveal remesh: crossing a threshold bumps resident generation counters exactly once.
- `every_screen_draws_text_at_one_size` / `..dims_and_panels_the_same_way` extended to the
  character screen and the rebuilt alchemy screen (three window sizes × four scales).
- `panel_height_is_the_same_on_both_tabs` (the bottom-anchored panel's top edge never
  jumps on a tab switch), `every_craftable_appears_in_exactly_one_family` (the screen's
  `FAMILIES` mirror can't silently miss a new `unlock::craft` entry), and
  `teaser_levels_match_the_core_tables` (§8.2's prose pinned to the enforcing functions).
- Loopback-transport e2e: XP totals survive rejoin (v4 records), legacy v3 record joins at
  level 1 with inventory intact, legacy sub-materials decode to essences.
- Netbench determinism: XP awards excluded from per-frame hashing or made order-stable
  (awards ride the action queue in wire order, so they should already be).

---

## 11. Milestones

Each milestone ships alone and is worth having alone. Two dependency inversions the
first sketch of this section got wrong: field extraction needs the **Essence Siphon**,
and the draw effect needs the **Stage-A emissive bit** — so the tool machinery and the
shader bit both move *ahead* of ore, and "catalog + tools" is no longer one phase.
Each entry lists the follow-on work the feature drags in that the design sections
above don't spell out — art, audio, shaders, parity — because that's where the real
schedule lives.

**M1 — Skills skeleton** *(medium)*. Curve + `Skill` enum in core, wire v16, dig-credit
and `process_alchemy` XP hooks, `inventories.bin` v4 + `state.json`, XP toasts,
character screen. Mining XP from ordinary digs only; zero balance change.
Hidden work: the `C` key is a new `Key` variant, which ripples through winit translation
in `digblock-platform`, the web client's keydown mapping, *and* a pause-menu row
(iOS has no keyboard — the menu row is the touch path, not a nicety). The character
screen must join the two pinned theme tests (3 window sizes × 4 scales) and get
`DIG_SCREEN=skills` stocking, or it ships invisible to capture like the two regressions
before it. The level-up sfx is the **first new audio asset**: CC0 sourcing, provenance
in `SOURCES.md`, the oggenc pipeline, embedding, and by-ear verification in *both*
fillers — audio has no headless test. Netbench needs the XP-award-ordering check (§10).

**M2 — Essence economy + Rubble + the alchemy screen redo** *(large — the economy
switches here)*. `Material` → `Essence` with decode-time legacy migration, extraction /
waste / bonus-line tables, the Rubble rule, the rebuilt two-tab screen with the
extraction slot, and the server-side extraction bucket (§8.1).
Hidden work: this milestone contains the project's **one destructive test change** —
the exact-closure economy pins (3 slabs = 27 = one block) are retired and every cost
test regrows around the waste multiplier; budget for a test-suite migration, not just
new pins. Rubble is the first new art (decide hand-authored vs. `texgen`-derived).
The screen is the largest UI job since the inventory: a progress-bar widget that
doesn't exist yet, drag-to-slot on **touch** (no hover states), captured-wheel scroll,
locked/OWNED rows, theme-test membership, `DIG_SCREEN=alchemy` re-stocked with
essences so captures show the new tabs. The per-cycle extraction tick wants a small
sfx (audio pipeline again). Loopback e2e for v3→v4 records and legacy sub-material
decode lands here, because this is the milestone that could corrupt somebody's stash.

**M3 — Mining speed** *(small)*. Level-scaled `required`, SIM 11. Hidden work: dig
completion ticks shift with level, so netbench re-baselines — the determinism gotcha
already on record. `dig_grace` absorbs mid-hold level-ups; verify with a test, not
by assertion. Note this is the plan's **one deliberately player-visible nerf**: §5.1's
below-baseline start slows every existing player down on day one, climbing back to
familiar speed at 30 — it ships *after* M1's XP system so the paying-back is already
running when the debt lands, and the patch note should say so in exactly those terms.

**M4 — Tool formalization + Stage A light + Torch** *(medium — pulled ahead of ore,
which needs both halves)*. The separated tool slot, tap-9 cycling, the faded rail,
tool-id migration of the `Layout` 9th entry; the emissive material bit; the Torch as
its first user (Alch 8 is pre-ore in the level table for exactly this reason).
Hidden work: the emissive bit is **cross-shader** — word 1's upper bits are shared
with the water payload, so the bit must be carved where the opaque path actually has
room, and chunk/shadow/water.wgsl must keep agreeing (the naga test catches invalid
WGSL, not wrong-looking light — screenshot captures are the visual gate, and Tint's
stricter rules mean the web build must be checked, not assumed). Decide shadow
behavior explicitly: emissive skips the *received* sun term but still casts.
Stored layouts in `player_store` carry the old `WIRE_TOOL` sentinel — the migration
is decode-time there too. Cycling needs its touch rule verified on device, and the
rail icons ride the existing `item_icon` sites. Torch is a new baked shape (floor
mount) with an emissive flame layer: shapes.rs authoring, `MAX_SHAPE_QUADS` pin,
one PNG.

**M5 — Ore** *(large — the headline)*. `ore_at` + golden pins + density histograms,
five block rows, reveal stamping + threshold remesh, the Siphon, field extraction,
DepletedStone, dual XP, the draw.
Hidden work: four ore textures + DepletedStone with **emissive seam layers** designed
to read at rest *and* under progress-driven bloom — the most art-directed assets in
the plan. The draw itself is three new render pieces: a world-space mote emitter (the
`spray.rs` mold, but a new instanced pipeline), the siphon viewmodel in `item_geom`
as the convergence point, and **per-cell progress-driven emissive**, which has no
existing plumbing — the crack-decal path is the nearest precedent and it's a decal,
not a material modulation; expect real design here. Remote players seeing the stream
means the wire must say *who is currently drawing on which cell* — the server knows
(DigStart), but today no snapshot field carries an active dig-hold; that's a small
append-only addition the doc's "no new wire" claim for extraction does **not** cover.
Audio: the siphon draw is a *sustained loop*, and the audio system is one-shots +
ambient beds — the waterfall-voice reconciliation is the only precedent for a held
voice; plus a completion burst and a refusal thunk for `DigRefused`. The reveal
remesh sweep needs its once-only pin, and the mesher snapshot grows a mining-level
field (plumbed like `EditOverlay`, wasm-inline path included).

**M6 — Resonant Pick + endgame lines** *(small-medium)*. The pick (begin_multi reuse,
server-side pair derivation, 1.5× timing, the Mining 85 extension), prospector's
sense, the 99 mastery line.
Hidden work: prospector's sense is a new *cadence audio emitter* driven by an
`ore_at` proximity scan — pure and cheap, but it's another sourced sfx and another
by-ear check. The pick's second/third dig cell interacts with protection and
`InsidePlayer` per-cell; the assembled-writes list covers it, but the test has to
prove that, not cite it.

**M7 — Catalog waves** *(open-ended, ships release by release)*. Materials and shapes
per §7.2's grid, functional items, glass, the lights.
Hidden work — this is where the follow-on tail is longest:
- **Every new shape is a shapes.rs project**, not a row: sub-box authoring, baked
  variants, `FaceMask` coverage, collision + interaction boxes, `state_valid` gates,
  a `MAX_SHAPE_QUADS` pin, facing tests. Trapdoor adds a **new facing axis** to the
  state nibble; Ladder needs the first new movement rule since swimming (§12 — ship
  decorative first or hold it); Wedge reuses ramp physics but must prove it.
- **Art is the long pole.** Counting §7 as written: Rubble, four ores, DepletedStone,
  Torch, Lantern, Aetherlamp, Framed Window, Glass, Stone Bricks, Polished Stone,
  Sandstone Bricks, Metal, plus a PNG per authored material × shape pair — dozens of
  textures, with `texgen` extended for the derived ones. The grid rule means the
  *code* scales; the art never does.
- **Audio material tables**: metal, glass, and rubble footsteps/dig sounds don't
  exist — every new block family needs a `materials` mapping in `audio.rs` and
  sourced sfx, or new blocks are silent in a way players notice immediately.
- **Glass** ships `Cutout` per §12's answer; revisiting translucency is a render
  design question with the water pass, not a catalog task.
- The 3:1 essence exchange and the Water craft are each likely **one append-only
  `AlchemyOp` arm** (essence→essence and essence→charge don't fit `Convert`'s
  block-in shape) — small, but wire-visible, so they belong to a planned protocol
  bump, not a patch.

**Cross-cutting, every milestone**: wasm-clean core changes (`just check-web`),
`just check-ios`, touch input for every new interaction, `DIG_SCREEN` stocking for
every new screen state, and a CLAUDE.md update as each phase's invariants land.
Balance is a workstream, not a milestone — every number above is a proposal, so plan
a bot-soak + playtest after M2 (the economy) and M5 (ore rates) specifically, since
those are the two milestones that set prices players will remember.

---

## 12. Open questions

- **Ore names.** Emberore/Duskstone/Glowvein/Aetherock are placeholders; four names carry
  a lot of the fantasy and appear in inventory, alchemy rows, and unlock text.
- **Essence display name for Metal** — "Metal" vs "Mineral" vs leaning into "Ore" despite
  the raw-ore collision.
- **Ladder physics** — climbable needs a new movement rule (the first since swimming);
  ship the shape as decorative first, or hold it for the physics?
- **Should placed ore blocks be diggable by below-gate players?** Current answer: yes,
  placed ore is ordinary chunk data (visible to all, drops itself). The alternative —
  reveal-gating placed ore too — breaks "chunk data is what everyone sees" for marginal
  gain.
- **Extraction queue depth** — one slot in v1; the natural high-level QoL is a second
  queue slot (finish this stack, start that one) rather than any "extract all" button,
  which would hand the picking back to the screen — the thing the slot model exists to
  avoid.
- **Curve steepness** — 1.1M total is ~40% of a RS 99. If the game grows more endgame
  content, revisit the `83`.
- **Glass render class.** `Translucent` currently means the specialized water pass
  (surface payloads, its own sorting story); glass through that pass is unproven. Safer
  v1: `Cutout` with a framed/latticed texture that reads as glass without per-pixel
  alpha blending; upgrade to true translucency when the render question is answered
  on its own terms.
- **Rubble scope.** Stone and Sandstone share one Rubble id (one texture, one row). If
  sandstone rubble wants its own warm-toned look later, a second id is append-only —
  but one rubble keeps the extraction table and the cobble aesthetic simple.
- **Torch mounting.** v1 is floor-mounted only (one baked shape, no facing states). Wall
  torches are a state-nibble variant like slab anchors when wanted.
- **Stage B lighting** — RESOLVED: landed as LIGHT-DESIGN.md (blocklight + skylight,
  §7.4). M5's ore-seam glitter still uses only the Stage-A ink bit, as designed.
