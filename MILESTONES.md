# Essence redesign — milestones

Branch `worktree-essence-nodes`. Small checkable milestones for the current stage; high-level placeholders for the
stages to come (expanded on entry). I check items off as they land.

## Stage 1 — nodes core (current)

- [x] **1.1 Block & material rows** — six new blocks (Heartwood/DrainedHeartwood, Leystone/SpentLeystone,
      Springstone/SpentSpringstone), texture layer ids, `Material` variants, fleck palettes (`node_flecks`),
      rosters/dig-family pins updated
- [x] **1.2 Textures** — texgen recipes appended, PNGs committed, assets `LAYERS` entries; texture pin green
- [x] **1.3 Ore rebanding** — every tier gets a floor beside its ceiling (depth predicts tier; Emberore confined to
      surface-cave depths), Leystone joins the deposit stream as the surface tier; band tests rewritten *(goldens
      re-pinned at the 1.8 gate)*
- [x] **1.4 Unified node layer** — `worldgen/nodes.rs`: `heartwood_at` / `springstone_at`, `revealed` / `dig_mode` /
      `spent_block` over all seven node kinds; `decoration::trunk_at`; purity + anti-dupe + roster tests
- [x] **1.5 Unlock axes** — `alchemy_reveal` (Heartwood 1 / Leystone 5 / Springstone 12), siphon → 1, torch → 10,
      `opens` counts surface reveals; unlock pins updated
- [x] **1.6 Yields + XP** — node yield bases *(done in 1.1)*, `field_yield` generalized to every node kind, `skills`
      field-XP table covering surface nodes; tests
- [x] **1.7 Reveal stamp** — `NodeStamp` carries both skill levels, mesher claims Wood/Sand hosts, pipeline
      `set_skill_levels`, `job_wire` roundtrip; stamp tests
- [x] **1.8 Stage gate** — `cargo test -p digblock-core` fully green, goldens deliberately re-pinned
- [x] **1.9 Call-site propagation** — engine prediction / GI LUT / audio materials / server sim moved onto the node
      layer (`node_dig_mode`, `Levels`, `spent_block`, `sync_node_reveal`); inventory grid grew a seventh row
      (`GRID_SLOTS`, `FIT_FLOOR` one rung lower); workspace check + clippy + fmt clean, core/net/render/server/persist
      tests green

## Stage 2 — extraction removal (current; swapped ahead of the recipe rebuild — repricing crafts

while bench extraction still pays would open a craft-cheap/extract-full matter pump)

- [x] **2.1 Core** — `Convert` leaves the core op; bench-extraction tables/paces/XP deleted (`bonus_lines`,
      `extract_worth`, `convert_xp`, the bucket-charge Aqua row); `node_yields` stays as the draw's table; extraction
      pins retired, payability pin rewritten over node sources
- [x] **2.2 Server** — `process_alchemy` refuses `Convert` with the standing rebase; bench bucket craft-only; sim tests
      rewritten (Craft-based XP test)
- [x] **2.3 Client** — EXTRACT tab and bucket tile removed (screen is transmute-only), `bench_tell` craft-only,
      engine/offline arm follows
- [x] **2.4 Protocol** — v27 + `MIN_PROTOCOL` raise + SIM bump, history comments
- [x] **2.5 Gate** — workspace tests green

## Stage 3 — recipe rebuild: feedstock + catalyst

- [x] **3.1 Material axes** — `Material::feedstock` (timbers ← Wood, masonry ← Rubble, sand-likes ← Sand, soils ←
      themselves; Metal/Crystal essence-only) and `craft_units` recast as the per-cube CATALYST for feedstock substances
- [x] **3.2 Batch + derivation** — `shapes::craft_batch` (one feedstock cube cuts a batch of the shape; conservation
      structural), `craft_ideal`/`craft_feedstock` derive the pair, functional reprices (torch/furniture/lamps gain
      feedstock lines), golden recipes re-pinned
- [x] **3.3 Rules + screen** — `apply_alchemy` debits feedstock all-or-nothing beside the catalyst, `craft_worth` prices
      matter, screen affordability/shortfall names feedstock; server/screen tests scaled to batches
- [x] **3.4 Gate** — clippy + fmt + full non-Docker test sweep green

## Stage 4 — identity, docs, captures

- [x] **4.1 Reagent identities** — display renames (VITAE / TERRA / UNDA / FERRUM / AETHER, indices frozen), teasers
      re-pinned, `/give` vocabulary follows by construction
- [x] **4.2 Captures** — `DIG_NODE=<heartwood|leystone|springstone>` scenes (ore chamber generalized; surface scenes
      write nothing); smoke-captured headlessly: heartwood trunk, springstone bed, and the reworked transmute page all
      photograph correctly
- [x] **4.3 Docs** — SKILLS-DESIGN §14 (the redesign: what landed, what it supersedes, open ends), CLAUDE.md
      economy/nodes/grid/tools/unlock rewrite, stale extraction comments swept from code
- [x] **4.4 Gate** — fmt + clippy + full non-Docker test sweep + `just check-ios` + check-web's wasm clippy green;
      `just check-md` green. NOT run here: the server e2e suite (Docker isn't running on this machine) and the Chrome
      shader check (no WGSL changed in the entire redesign)
