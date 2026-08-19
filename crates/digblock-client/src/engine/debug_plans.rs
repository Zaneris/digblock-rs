//! Debug capture scenes: the block plans behind `DIG_SHAPES`, `DIG_ORE`,
//! `DIG_VAULT` and the rest (CLAUDE.md's capture flags).
//!
//! Pure `(spawn, seed) -> Vec<(x, y, z, cell)>` builders with no `Engine`
//! coupling at all, which is why they live apart from it. Every scene here
//! exists because the thing it photographs is otherwise unreachable to a
//! headless capture — a page that is input-only, an effect that is over in
//! a second, or a level a fresh player does not have.

use digblock_core::BlockType;
use glam::Vec3;

/// Stair/door placement facing from the look direction: the dominant
/// horizontal axis, ascending/facing AWAY from the placer. The server
/// accepts the client-chosen facing (validated by `state_valid`), so no
/// re-derivation can disagree.
pub(super) fn quantized_facing(dir: Vec3) -> digblock_core::shapes::Facing {
	use digblock_core::shapes::Facing;
	if dir.x.abs() >= dir.z.abs() {
		if dir.x >= 0.0 {
			Facing::PosX
		} else {
			Facing::NegX
		}
	} else if dir.z >= 0.0 {
		Facing::PosZ
	} else {
		Facing::NegZ
	}
}

/// Every authored block of one shape family, in material order — the
/// showcase scenes walk this so a new material × shape pair photographs
/// itself. The old scenes hand-listed `[WoodStairs, StoneStairs,
/// LumberStairs]`, which is exactly the kind of mirror §7.2's grid exists
/// to delete: two timbers landed and neither appeared in any capture.
pub(super) fn shaped_roster(s: digblock_core::block::ShapeFamily) -> Vec<BlockType> {
	digblock_core::material::Material::ALL
		.into_iter()
		.filter_map(|m| BlockType::from_grid(m, s))
		.collect()
}

/// DIG_SHAPES=1 layout: every shaped variant in rows floating at eye
/// height near spawn (render verification needs no ground) — every stair
/// material × 4 facings, then closed and open doors in every material,
/// then the slabs. Rows run back in z, one material per row, so the rank
/// grows backward as the catalog does instead of off the sides of the
/// frame.
pub(super) fn debug_shapes_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::{BlockType as B, ShapeFamily};
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{Facing, door_state, slab_state, stair_state};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	let stairs = shaped_roster(ShapeFamily::Stairs);
	let doors = shaped_roster(ShapeFamily::Door);
	let slabs = shaped_roster(ShapeFamily::Slab);
	// One z cursor, walked front to back, so a row added anywhere pushes
	// what follows instead of colliding with it. Nearest first is what the
	// camera actually frames: the FURNITURE rides right behind the stairs
	// because it is the newest thing to look at.
	let mut z = bz + 2;
	for (j, &b) in stairs.iter().enumerate() {
		for (i, f) in Facing::ALL.into_iter().enumerate() {
			out.push((
				bx + 2 * i as i32 - 3,
				by,
				z + 2 * j as i32,
				make_cell(b, stair_state(f)),
			));
		}
	}
	z += 2 * stairs.len() as i32;
	// Furniture × 4 facings: the table is stateless, so its row is four
	// identical tops — which is the point, since pushed together they read
	// as one surface. The chair and the bed turn.
	for (j, b) in [B::Table, B::Chair, B::Bed].into_iter().enumerate() {
		for (i, f) in Facing::ALL.into_iter().enumerate() {
			let state = if b.takes_facing() { stair_state(f) } else { 0 };
			out.push((
				bx + 2 * i as i32 - 3,
				by,
				z + 2 * j as i32,
				make_cell(b, state),
			));
		}
	}
	z += 6;
	// Doors take two z rows each (closed, then open), at the two heights a
	// leaf spans, so both states of every material are in one capture.
	for (j, &b) in doors.iter().enumerate() {
		for (i, f) in Facing::ALL.into_iter().enumerate() {
			for (dz, open) in [(0, false), (2, true)] {
				for upper in [false, true] {
					out.push((
						bx + 2 * i as i32 - 3,
						by + i32::from(upper),
						z + 4 * j as i32 + dz,
						make_cell(b, door_state(f, open, upper)),
					));
				}
			}
		}
	}
	z += 4 * doors.len() as i32;
	let slab_z = z;
	// A torch row: one on the ground, one up on a wood plinth — the
	// emissive head is the thing to photograph at night.
	out.push((bx - 5, by, bz + 4, B::Torch as u16));
	out.push((bx - 5, by, bz + 6, B::Wood as u16));
	out.push((bx - 5, by + 1, bz + 6, B::Torch as u16));
	// Slabs: both materials × both states, then a walkable seam row —
	// 1/3 | 2/3 | full block | full + raised 1/3, every riser one
	// auto-step (also eyeballs boundary culling on the shared planes).
	// Ceiling slabs hang two cells up, where their underside is the
	// thing to look at.
	for (i, b) in slabs.into_iter().enumerate() {
		let x = bx + 2 * i as i32 - 3;
		out.push((x, by, slab_z, make_cell(b, slab_state(false, false))));
		out.push((x, by, slab_z + 2, make_cell(b, slab_state(true, false))));
		out.push((x, by + 2, slab_z, make_cell(b, slab_state(false, true))));
		out.push((x, by + 2, slab_z + 2, make_cell(b, slab_state(true, true))));
	}
	let base = [
		make_cell(B::WoodSlab, slab_state(false, false)),
		make_cell(B::WoodSlab, slab_state(true, false)),
		B::Wood as u16,
		B::Wood as u16,
	];
	for (dx, cell) in base.into_iter().enumerate() {
		out.push((bx + 1 + dx as i32, by, slab_z + 4, cell));
	}
	out.push((
		bx + 4,
		by + 1,
		slab_z + 4,
		make_cell(B::WoodSlab, slab_state(false, false)),
	));
	out
}

/// DIG_ORE=<tier 1..=4>: a chamber hollowed out around the nearest
/// deposit of that tier, the vein left standing in its wall, and a torch
/// on the floor to see it by. Returns the plan and where to stand.
///
/// Ore is DERIVED and invisible below its level, which makes it the least
/// photographable thing in the game: there is no cell to teleport to,
/// because the cell is Stone until someone with the level looks at it.
/// Hence a scene rather than a pose — and hence the one rule that makes
/// the scene work at all: **the carve must not touch the vein**. An edited
/// cell never shows ore (that is the anti-dupe), so hollowing the box
/// naively would delete the very thing being photographed. Every cell the
/// deposit claims is left exactly as worldgen made it.
/// The reach of `DIG_ORE`'s chamber, in cells either side of the vein.
///
/// Module-scoped so the capture test can read the footprint it has to
/// reason about instead of keeping a copy — a copy that drifted would
/// have the test checking a box the plan never carved.
pub(super) const ORE_CHAMBER_R: i32 = 5;

pub(super) fn debug_ore_plan(
	spawn: Vec3,
	seed: f64,
	want: BlockType,
) -> (Vec3, Vec<(i32, i32, i32, u16)>) {
	use digblock_core::worldgen::{self, ore};
	let (sx, sz) = (spawn.x.floor() as i32, spawn.z.floor() as i32);
	// Nearest deposit of the tier — a search box wide enough that even the
	// rarest tier is found from any spawn.
	//
	// It has to be BURIED, and that is not automatic: a deposit is placed by a
	// hash over a grid that knows nothing about terrain, so a shallow band
	// routinely rolls one into open air above the surface — or under a lake.
	// `apply_nodes` stamps only cells that hold pristine Stone, so such a
	// deposit reveals nothing whatever level is granted, and the capture is an
	// empty stone-plated box that looks like a stamp bug. Every band grew a
	// floor in the redesign, which is what made this common.
	//
	// Distance is 3D for the same reason it is a scene at all: with the
	// vertical term the pick prefers a deposit at eye height over one the same
	// distance away and 80 blocks down.
	// The chamber: a box around the vein, minus the vein itself. Stood off far
	// enough on -z that the camera is outside it looking in.
	const R: i32 = ORE_CHAMBER_R;
	let buried = |d: &ore::Deposit| {
		// Clear of the surface by the CHAMBER's reach, not the blob's: the
		// carve is what would open the room to the sky, and it reaches
		// further than the vein does.
		//
		// Against `height` alone, which is the whole of the question.
		// Water fills `height + 1 ..= water` and never below, so a cell
		// under the surface is under rock whatever the column's water
		// says — testing `min(height, water)` instead would refuse every
		// dry deposit inside a hill standing above SEA_LEVEL, which is
		// most of the shallow band.
		d.y + R < worldgen::column_shape(d.x, d.z, seed).height
	};
	let mut best: Option<(i64, ore::Deposit)> = None;
	for d in ore::deposits_near((sx - 320, 1, sz - 320), (sx + 320, 200, sz + 320), seed) {
		if d.ore != want || !buried(&d) {
			continue;
		}
		let (dx, dy, dz) = (
			i64::from(d.x - sx),
			i64::from(d.y - spawn.y.floor() as i32),
			i64::from(d.z - sz),
		);
		let d2 = dx * dx + dy * dy + dz * dz;
		if best.is_none_or(|(b, _)| d2 < b) {
			best = Some((d2, d));
		}
	}
	let Some((_, d)) = best else {
		log::warn!("DIG_ORE: no buried {want:?} within reach of spawn");
		return (spawn, Vec::new());
	};
	let mut plan = Vec::new();
	for y in (d.y - R)..=(d.y + R) {
		for z in (d.z - R)..=(d.z + R) {
			for x in (d.x - R)..=(d.x + R) {
				if y <= digblock_core::block::BEDROCK_Y || ore::ore_at(x, y, z, seed).is_some() {
					continue;
				}
				plan.push((x, y, z, BlockType::Air as u16));
			}
		}
	}
	// A floor two below the vein, so a standing player's EYE lands level with
	// it — physics drops the camera onto that floor within a frame or two, and
	// a pose that has to be looked up at is a pose nobody frames correctly.
	//
	// Guarded exactly like the carve, and for the same reason: this plate is
	// laid THROUGH the blob. A max-radius deposit reaches `ore::MAX_RADIUS`
	// straight down, so the cell right under the centre sits on the rim, and
	// the plan is applied through `pipeline.set_cell`, after which
	// `nodes::revealed` answers None for that cell forever. An unguarded plate
	// quietly deletes the vein it was laid down to photograph.
	//
	// Dropping the whole plane clear of the deposit instead would not replace
	// this guard and would cost the pose: `ore_at` answers for every deposit in
	// the 3x3x3 apron, so a neighbour's blob can cross whatever plane we pick,
	// and a floor a block lower puts the eye UNDER the vein. Where a vein does
	// cross, the cell keeps its pristine worldgen value — ore-bearing Stone —
	// so the floor stays walkable and the seam runs through it.
	// Never at or below the world floor. `deposit_in` keeps a whole BLOB clear
	// of `BEDROCK_Y + MAX_RADIUS`, so this `max` cannot bite today; it is here
	// because that is a coupling two modules apart. Clamped rather than skipped
	// so the plate stays whole.
	let floor = (d.y - 2).max(digblock_core::block::BEDROCK_Y + 1);
	for z in (d.z - R)..=(d.z + R) {
		for x in (d.x - R)..=(d.x + R) {
			if ore::ore_at(x, floor, z, seed).is_some() {
				continue;
			}
			plan.push((x, floor, z, BlockType::Stone as u16));
		}
	}
	// A chamber this deep is lit by nothing at all and photographs as
	// black; the torch is off to one side so the vein is raked rather
	// than flattened by a light behind the camera.
	plan.push((d.x - 3, floor + 1, d.z - 3, BlockType::Torch as u16));
	log::info!("DIG_ORE: {want:?} vein at {},{},{}", d.x, d.y, d.z);
	(
		Vec3::new(
			d.x as f32 + 0.5,
			floor as f32 + 1.0 + digblock_core::player::EYE_TO_FEET,
			d.z as f32 - 4.5,
		),
		plan,
	)
}

/// `DIG_NODE`'s launcher-side parser, beside [`parse_held_item`] for the
/// same reason: the launchers do not link core, and the roster belongs
/// with the scene that consumes it.
pub fn parse_node_kind(v: &str) -> Option<BlockType> {
	Some(match v {
		"heartwood" => BlockType::Heartwood,
		"leystone" => BlockType::Leystone,
		"springstone" => BlockType::Springstone,
		_ => return None,
	})
}

/// DIG_NODE=<heartwood|leystone|springstone>: pose the camera at the
/// nearest node of that kind, with the reveal granted so the stamp shows
/// it. Leystone rides the deposit stream and reuses the ore chamber; the
/// two surface kinds are outdoor finds, so their scenes WRITE nothing —
/// a node is exactly as photographable as the world makes it, which is
/// the point of the capture.
pub(super) fn debug_node_plan(
	spawn: Vec3,
	seed: f64,
	node: BlockType,
) -> (Vec3, Vec<(i32, i32, i32, u16)>) {
	use digblock_core::worldgen::nodes;
	let (sx, sz) = (spawn.x.floor() as i32, spawn.z.floor() as i32);
	let eye = digblock_core::player::EYE_TO_FEET;
	match node {
		BlockType::Leystone => debug_ore_plan(spawn, seed, BlockType::Leystone),
		BlockType::Heartwood => {
			// The nearest heart-bearing trunk cell: trunks are a pure
			// candidate rule, so this is a column walk over the memoized
			// decoration cells — a one-time startup cost.
			let mut best: Option<(i64, (i32, i32, i32))> = None;
			for z in (sz - 160)..(sz + 160) {
				for x in (sx - 160)..(sx + 160) {
					let Some(t) = digblock_core::decoration::trunk_at(x, z, seed) else {
						continue;
					};
					if !nodes::heartwood_at(x, t.y0 + 1, z, seed) {
						continue;
					}
					let (dx, dz) = (i64::from(x - sx), i64::from(z - sz));
					let d2 = dx * dx + dz * dz;
					if best.is_none_or(|(b, _)| d2 < b) {
						best = Some((d2, (x, t.y0 + 1, z)));
					}
				}
			}
			let Some((_, (x, y, z))) = best else {
				log::warn!("DIG_NODE: no heartwood within reach of spawn");
				return (spawn, Vec::new());
			};
			log::info!("DIG_NODE: heartwood at {x},{y},{z}");
			// Stand a few blocks south of the trunk on the tree's own
			// ground, eye level with the heart.
			let ground = digblock_core::worldgen::terrain_height(x, z - 3, seed);
			(
				Vec3::new(x as f32 + 0.5, ground as f32 + 1.0 + eye, z as f32 - 2.5),
				Vec::new(),
			)
		}
		BlockType::Springstone => {
			let mut best: Option<(i64, (i32, i32, i32))> = None;
			for z in (sz - 512)..(sz + 512) {
				for x in (sx - 512)..(sx + 512) {
					// `springstone_bed` puts the rate hash genuinely first
					// and answers the bed height in the same call — this
					// loop used to evaluate `column_shape` for every
					// column of the box BEFORE the hash, and then pay for
					// it a second time inside `springstone_at`.
					let Some(y) = nodes::springstone_bed(x, z, seed) else {
						continue;
					};
					let (dx, dz) = (i64::from(x - sx), i64::from(z - sz));
					let d2 = dx * dx + dz * dz;
					if best.is_none_or(|(b, _)| d2 < b) {
						best = Some((d2, (x, y, z)));
					}
				}
			}
			let Some((_, (x, y, z))) = best else {
				log::warn!("DIG_NODE: no springstone within reach of spawn");
				return (spawn, Vec::new());
			};
			log::info!("DIG_NODE: springstone at {x},{y},{z}");
			// Stand on the nearest dry column shoreward of the spring,
			// looking over the water at the glint in the bed.
			let stand = (1..=12)
				.map(|d| (x, z - d))
				.find(|&(cx, cz)| {
					let s = digblock_core::worldgen::column_shape(cx, cz, seed);
					s.height >= s.water
				})
				.unwrap_or((x, z - 4));
			let ground = digblock_core::worldgen::terrain_height(stand.0, stand.1, seed);
			(
				Vec3::new(
					stand.0 as f32 + 0.5,
					ground as f32 + 1.0 + eye,
					stand.1 as f32 + 0.5,
				),
				Vec::new(),
			)
		}
		other => {
			log::warn!("DIG_NODE: {other:?} is not a surface node kind");
			(spawn, Vec::new())
		}
	}
}

/// DIG_VAULT=1 layout: a sealed stone vault enclosing the spawn camera,
/// one torch on the floor ahead and a stair to catch its falloff. The
/// NEE capture: skylight is stripped by the roof (the placement writes
/// ride the ordinary edit hook), so the interior photographs DARK except
/// the torch pool — at noon.
pub(super) fn debug_vault_plan(spawn: Vec3, torch: bool) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{Facing, stair_state};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// Shell-first, carve-second ordering doesn't matter — placement is
	// retried per cell and the light BFS settles under maintain either
	// way. The interior is a box around the eye cell, reaching further
	// AHEAD in z than behind so the torch pool has room to fall off.
	for x in (bx - 4)..=(bx + 4) {
		for y in (by - 3)..=(by + 3) {
			for z in (bz - 3)..=(bz + 9) {
				let interior = (bx - 3..=bx + 3).contains(&x)
					&& (by - 2..=by + 2).contains(&y)
					&& (bz - 2..=bz + 8).contains(&z);
				out.push((
					x,
					y,
					z,
					if interior {
						B::Air as u16
					} else {
						B::Stone as u16
					},
				));
			}
		}
	}
	// The torch, and something with depth for its light to model. The
	// torch is optional (DIG_VAULT_DARK) because a room whose ONLY source
	// is the one being carried is the capture a held light needs — with
	// the floor torch present the two pools cannot be told apart. Keeping
	// it is the other half of the pair: same lamp, placed and carried, in
	// one frame, so a calibration drift between them is visible.
	if torch {
		out.push((bx, by - 2, bz + 6, B::Torch as u16));
	}
	out.push((
		bx - 2,
		by - 2,
		bz + 5,
		make_cell(B::StoneStairs, stair_state(Facing::PosX)),
	));
	out.push((bx + 2, by - 2, bz + 4, B::Wood as u16));
	out
}

/// DIG_TUNNEL=1 layout: a long sealed stone tunnel running ahead of the
/// spawn camera, no torch — the unproven-daylight capture (GI-DESIGN.md
/// R32). The length is the scene: the far end sits beyond the probe
/// cascades' coverage and beyond the gather ray's reach, so every
/// "answer sky on no data" fallback fires along it, which no room-sized
/// scene can reach. Combine with `--pos` at a low y for the depth-gated
/// fade; a run above `SKY_FADE_FULL_Y` is the geometry-identical
/// control, since the fade saturates there.
pub(super) fn debug_tunnel_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// A one-block stone shell around a bore the player's capsule fits
	// (eye at `by`, feet two cells down), reaching far enough ahead in z
	// that the far wall photographs through every fallback regime at
	// once: past C0's coverage, past the gather ray, and short of the
	// window's edge.
	for x in (bx - 2)..=(bx + 2) {
		for y in (by - 3)..=(by + 1) {
			for z in (bz - 3)..=(bz + 53) {
				let interior = (bx - 1..=bx + 1).contains(&x)
					&& (by - 2..=by).contains(&y)
					&& (bz - 2..=bz + 52).contains(&z);
				out.push((
					x,
					y,
					z,
					if interior {
						B::Air as u16
					} else {
						B::Stone as u16
					},
				));
			}
		}
	}
	out
}

/// DIG_DIVE=1 layout: an open-topped stone tank flooded around the
/// spawn camera, so the eye starts one block below the surface — the
/// RTGI underwater capture. Open sky above is the light source: the
/// sun DDA crosses the water column (transmittance dims submerged
/// terrain), the probes see the pool through the same window, and the
/// surface overhead photographs as a plane from below (Snell window /
/// TIR mirror). Furniture on the floor gives the light something to
/// model.
pub(super) fn debug_dive_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{Facing, stair_state};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// A stone shell reaching further ahead in z than behind, its interior
	// carved then flooded to one block ABOVE the eye cell, so the
	// camera is submerged with the surface in view. No ceiling — the
	// sky lights the pool.
	for x in (bx - 4)..=(bx + 4) {
		for y in (by - 4)..=(by + 2) {
			for z in (bz - 4)..=(bz + 10) {
				let interior = (bx - 3..=bx + 3).contains(&x)
					&& (by - 3..=by + 2).contains(&y)
					&& (bz - 3..=bz + 9).contains(&z);
				let cell = if !interior {
					B::Stone as u16
				} else if y <= by + 1 {
					B::Water as u16
				} else {
					B::Air as u16
				};
				out.push((x, y, z, cell));
			}
		}
	}
	// Submerged furniture: sand strip, a stair, a wood pillar.
	for dz in 2..=4 {
		out.push((bx - 1, by - 3, bz + dz, B::Sand as u16));
	}
	out.push((
		bx + 1,
		by - 3,
		bz + 4,
		make_cell(B::StoneStairs, stair_state(Facing::PosX)),
	));
	out.push((bx + 2, by - 3, bz + 6, B::Wood as u16));
	out.push((bx + 2, by - 2, bz + 6, B::Wood as u16));
	out
}

/// DIG_RIVER=1 layout: a wide, deep flooded basin AHEAD of the spawn
/// camera (yaw 0 faces +z), its surface a few blocks below the eye — the
/// "standing at a river's edge looking into it" frame-cost scene (R17).
/// Pair with `--pitch -35` so near water fills most of the screen. All
/// of it sits inside C0 beside bank geometry (the expensive gather
/// path), and the submerged stone floor keeps the resolve's rays
/// crossing the water column, which is the other half of that scene's
/// cost.
pub(super) fn debug_river_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	for x in (bx - 25)..=(bx + 25) {
		for y in (by - 10)..=(by + 8) {
			for z in (bz + 2)..=(bz + 52) {
				let interior = (bx - 24..=bx + 24).contains(&x)
					&& y >= by - 9 && (bz + 3..=bz + 51).contains(&z);
				let cell = if y >= by - 2 {
					// Open air above the waterline — clears any hillside
					// so the whole surface is in view from the rim.
					B::Air as u16
				} else if interior {
					B::Water as u16
				} else {
					// Shell: floor and a one-block rim at water level, so
					// the pool cannot drain and the banks give C0 probes
					// geometry.
					B::Stone as u16
				};
				out.push((x, y, z, cell));
			}
		}
	}
	out
}

/// DIG_CRACK=&lt;p&gt; layout: a flat wall of blocks two cells ahead of the
/// spawn camera, tall and wide enough that the crosshair lands on one
/// whatever the eye height resolves to.
///
/// Dirt, not Stone: the decal only draws on a cell `diggable` says yes
/// to, and rock is behind the Mining gate, so a fresh capture profile
/// would photograph nothing at all. The neighbours either side are a
/// second material because the crack is seeded per (cell, face) — three
/// blocks in one frame is what shows the pattern is not one stamp.
pub(super) fn debug_crack_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	for x in (bx - 2)..=(bx + 2) {
		for y in (by - 3)..=(by + 2) {
			let b = if x == bx { B::Dirt } else { B::Lumber };
			out.push((x, y, bz + 2, b as u16));
		}
	}
	out
}

/// DIG_DOORS=1 layout: a rank of door pairs standing three cells ahead of the
/// spawn camera with nothing in front of them, plus a plain Lumber column beside
/// them as the tint reference.
///
/// DIG_SHAPES already stands every door variant, so this looks like a duplicate
/// and is not: it stands them EIGHT blocks out behind three rows of stairs
/// sharing their x columns, and from eye height a one-block stair projects
/// straight over the lower half of the door behind it. The seam between a door's
/// two cells is the one thing a door has to be judged on — one object wearing
/// two cells, so the tint has to match across it and the outline has to wrap
/// both. Three cells out also puts the pair inside the interact ray.
///
/// The reference column is deliberately the door's OWN material: the leaf must
/// read as one piece of timber down its own middle while still differing from
/// the timber beside it.
pub(super) fn debug_doors_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	use digblock_core::chunk::make_cell;
	use digblock_core::shapes::{Facing, door_state};
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// NegZ faces the camera: the leaf sits against the cell face the
	// facing points at, and the camera is on the −z side.
	for (dx, f, open) in [
		(0, Facing::NegZ, false),
		(2, Facing::NegZ, true),
		(-2, Facing::PosX, false),
	] {
		for upper in [false, true] {
			out.push((
				bx + dx,
				by + i32::from(upper),
				bz + 3,
				make_cell(B::LumberDoor, door_state(f, open, upper)),
			));
		}
	}
	for y in by..=(by + 1) {
		out.push((bx - 3, y, bz + 3, B::Lumber as u16));
		out.push((bx + 3, y, bz + 3, B::Lumber as u16));
	}
	out
}

/// DIG_ROOF=1 layout: a wide stone plate hanging over the ground ahead
/// of spawn with NO walls — the beam-veto capture. Skylight under it
/// drops (lateral spread decays toward the middle), so this scene is
/// exactly where scaling direct sun by the nibble went wrong: a morning
/// sun slants in from the open side and must light the ground precisely
/// where the shadow map says, with only the ambient dome dimming.
pub(super) fn debug_roof_plan(spawn: Vec3) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	for x in (bx - 8)..=(bx + 8) {
		for z in (bz + 2)..=(bz + 18) {
			out.push((x, by + 3, z, B::Stone as u16));
		}
	}
	out
}

/// DIG_TORCHES=&lt;n&gt; layout: an open-topped stone courtyard ahead of spawn
/// with `n` torches clustered at its near end and forty blocks of bare floor
/// running away from them.
///
/// DIG_VAULT cannot photograph the remaining local-light noise, for two reasons
/// that are the whole point of this scene. Its room is eleven blocks deep, so
/// every surface sits inside `LIGHT_RANGE` and well inside the near-field clamp
/// — there is no "far from the torch" band at all. And it holds exactly ONE
/// torch, the single case the resolve's NEE covers completely.
///
/// So: `n` is a knob (1 vs 8 with the geometry held fixed) and the floor runs
/// well past `LIGHT_RANGE`, so a capture crosses that edge with the same
/// material and face code either side. No roof — the report is about NIGHT, so
/// `--tod 180` is the pose and `--tod 0` the control.
pub(super) fn debug_torches_plan(
	spawn: Vec3,
	n: i32,
	far: i32,
	wall: bool,
) -> Vec<(i32, i32, i32, u16)> {
	use digblock_core::block::BlockType as B;
	let (bx, by, bz) = (
		spawn.x.floor() as i32,
		spawn.y.floor() as i32,
		spawn.z.floor() as i32,
	);
	let mut out = Vec::new();
	// Floor laid to DIG_VAULT's convention — one cell below the first air
	// cell — and cleared far enough overhead that nothing worldgen left
	// there shadows the run.
	for x in (bx - 16)..=(bx + 16) {
		for z in (bz - 6)..=(bz + 46) {
			out.push((x, by - 3, z, B::Stone as u16));
			for y in (by - 2)..=(by + 8) {
				let wall = x == bx - 16 || x == bx + 16 || z == bz + 46 || z == bz - 6;
				out.push((
					x,
					y,
					z,
					if wall && y <= by + 2 {
						B::Stone as u16
					} else {
						B::Air as u16
					},
				));
			}
		}
	}
	// The cluster: the `SPOTS` roster, spaced a few blocks apart, laid down
	// in a fixed order so `n` = 1 keeps the same first torch in the same
	// cell as `n` = 8 and the A/B moves one variable.
	//
	// Parked well down the courtyard rather than at the camera's feet,
	// because the report is about distance: an eye-height camera at spawn
	// then holds all three regimes in one frame — near floor lit only by
	// what reaches it (the band the artifact is reported in, and the one a
	// half-res texel magnifies most), the pools themselves, and floor
	// running out past `LIGHT_RANGE` behind them where no NEE fires at all.
	const SPOTS: [(i32, i32); 8] = [
		(0, 17),
		(3, 17),
		(-3, 17),
		(0, 20),
		(3, 20),
		(-3, 20),
		(0, 14),
		(3, 14),
	];
	for &(dx, dz) in SPOTS.iter().take(n.clamp(0, 8) as usize) {
		out.push((bx + dx, by - 2, bz + dz, B::Torch as u16));
	}
	// `wall` cuts the cluster in half with a solid screen, so a floor
	// point on either side sees some of the torches and not the others.
	//
	// That is the one case a resampled NEE cannot price: the draw is
	// proportional to UNSHADOWED contribution, so a point that can only
	// see half of what it samples gets a Bernoulli on the pool total. The
	// open courtyard cannot show it — with every emitter visible the
	// estimator is exact — so without this the residual would have to be
	// argued rather than measured.
	if wall {
		for dy in 0..4 {
			for dx in -7..=7 {
				out.push((bx + dx, by - 2 + dy, bz + 18, B::Stone as u16));
			}
		}
	}
	// `far` torches on a pad well BEHIND the camera, outside the
	// courtyard wall. They cannot light one pixel of the capture — every
	// visible surface is past `LIGHT_RANGE` from them and the wall is in
	// the way — but they are well inside the reach `flush_lights` keeps,
	// so they land in the light LIST.
	//
	// That is the entire experiment. The resolve scans the list by
	// position, so it does not care; gi_probe's `nee` picks ONE entry
	// UNIFORMLY over the whole list and divides by 1/count, so every entry
	// that cannot contribute still takes its turn and multiplies the ones
	// that can. If flicker tracks `far` with the picture held fixed, the
	// probe cache's light selection is a source in its own right.
	for i in 0..far.clamp(0, 64) {
		let x = bx - 6 + (i % 8) * 3;
		let z = bz - 45 - (i / 8) * 3;
		out.push((x, by - 3, z, B::Stone as u16));
		out.push((x, by - 2, z, B::Torch as u16));
	}
	out
}
