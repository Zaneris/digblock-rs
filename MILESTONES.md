# Essence redesign — milestones

Branch `worktree-essence-nodes`. Small checkable milestones for the current stage; high-level
placeholders for the stages to come (expanded on entry). I check items off as they land.

## Stage 1 — nodes core (current)

- [x] **1.1 Block & material rows** — six new blocks (Heartwood/DrainedHeartwood,
      Leystone/SpentLeystone, Springstone/SpentSpringstone), texture layer ids, `Material` variants,
      fleck palettes (`node_flecks`), rosters/dig-family pins updated
- [x] **1.2 Textures** — texgen recipes appended, PNGs committed, assets `LAYERS` entries; texture pin
      green
- [x] **1.3 Ore rebanding** — every tier gets a floor beside its ceiling (depth predicts tier;
      Emberore confined to surface-cave depths), Leystone joins the deposit stream as the surface
      tier; band tests rewritten *(goldens re-pinned at the 1.8 gate)*
- [x] **1.4 Unified node layer** — `worldgen/nodes.rs`: `heartwood_at` / `springstone_at`,
      `revealed` / `dig_mode` / `spent_block` over all seven node kinds; `decoration::trunk_at`;
      purity + anti-dupe + roster tests
- [x] **1.5 Unlock axes** — `alchemy_reveal` (Heartwood 1 / Leystone 5 / Springstone 12), siphon → 1,
      torch → 10, `opens` counts surface reveals; unlock pins updated
- [x] **1.6 Yields + XP** — node yield bases *(done in 1.1)*, `field_yield` generalized to every node
      kind, `skills` field-XP table covering surface nodes; tests
- [x] **1.7 Reveal stamp** — `NodeStamp` carries both skill levels, mesher claims Wood/Sand hosts,
      pipeline `set_skill_levels`, `job_wire` roundtrip; stamp tests
- [x] **1.8 Stage gate** — `cargo test -p digblock-core` fully green, goldens deliberately re-pinned
- [x] **1.9 Call-site propagation** — engine prediction / GI LUT / audio materials / server sim moved
      onto the node layer (`node_dig_mode`, `Levels`, `spent_block`, `sync_node_reveal`); inventory
      grid grew a seventh row (`GRID_SLOTS`, `FIT_FLOOR` one rung lower); workspace check + clippy +
      fmt clean, core/net/render/server/persist tests green

## Stage 2 — extraction removal (current; swapped ahead of the recipe rebuild — repricing crafts
while bench extraction still pays would open a craft-cheap/extract-full matter pump)

- [x] **2.1 Core** — `Convert` leaves the core op; bench-extraction tables/paces/XP deleted
      (`bonus_lines`, `extract_worth`, `convert_xp`, the bucket-charge Aqua row); `node_yields` stays
      as the draw's table; extraction pins retired, payability pin rewritten over node sources
- [x] **2.2 Server** — `process_alchemy` refuses `Convert` with the standing rebase; bench bucket
      craft-only; sim tests rewritten (Craft-based XP test)
- [x] **2.3 Client** — EXTRACT tab and bucket tile removed (screen is transmute-only),
      `bench_tell` craft-only, engine/offline arm follows
- [x] **2.4 Protocol** — v27 + `MIN_PROTOCOL` raise + SIM bump, history comments
- [x] **2.5 Gate** — workspace tests green

## Stage 3 — recipe rebuild: feedstock + catalyst (placeholder)

- [ ] `Material::feedstock` + `catalyst_units`, `shapes::craft_batch`, `CraftPrice` through
      `craft_ideal`/`apply_alchemy`/paces, functional reprices (siphon & bucket feedstock-only —
      the essence tool never costs essence), new pins (no-craft-creates-essence, feedstock
      conservation, payability v2), screen affordability/detail strip
- [ ] Gate: core + net + render + server tests green

## Stage 4 — identity, docs, captures (placeholder)

- [ ] Essence display renames (all five, Metal included), SKILLS-DESIGN §14 + CLAUDE.md rewrite,
      `DIG_NODE` capture scenes, balance instrumentation notes
- [ ] Gate: `just check` + `just check-ios` + `just check-web`
