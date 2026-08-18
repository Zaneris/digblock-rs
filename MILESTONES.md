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

## Stage 2 — recipe rebuild (placeholder)

- [ ] Feedstock + catalyst recipes (`Material::feedstock`, small essence lines, `craft_batch`),
      `apply_alchemy` rework, pace/worth rebuild, functional reprices (siphon/bucket feedstock-only —
      the essence tool never costs essence)
- [ ] Gate: core + net tests green

## Stage 3 — extraction removal (placeholder)

- [ ] Extract tab removed (screen becomes transmute-only), server refuses `Convert`, offline mirror,
      dig-credit node-draw arm generalized, protocol v27 + `MIN_PROTOCOL` raise, SIM bump
- [ ] Gate: workspace tests + `just check-web`

## Stage 4 — identity, docs, captures (placeholder)

- [ ] Essence display renames (all five, Metal included), SKILLS-DESIGN §14 + CLAUDE.md rewrite,
      `DIG_NODE` capture scenes, balance instrumentation notes
- [ ] Gate: `just check` + `just check-ios` + `just check-web`
