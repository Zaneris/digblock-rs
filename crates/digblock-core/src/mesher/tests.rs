use super::*;
use crate::block::BlockType as B;

fn solo(block: B) -> ChunkData {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, block);
	d
}

/// The host mask `Pipeline::node_stamp` would hand a padded box filled
/// with one block — the tests stand in for the main thread's scan.
fn hosts_of(host: B) -> NodeHosts {
	NodeHosts {
		deposits: host == B::Stone,
		heartwood: host == B::Wood,
		springs: host == B::Sand,
	}
}

/// Every neighbor missing — the old ALL_MISSING fixture.
fn isolated(d: &ChunkData) -> MeshInput {
	MeshInput::build(d, |_, _, _| Src::Missing)
}

/// The reveal stamp and the DIG path must answer identically for every cell — one is a worker
/// rewriting block ids, the other `nodes::revealed` asked once at a dig, and a disagreement is a
/// node you can see and cannot extract, or one you cannot see and accidentally do. Swept over a
/// whole padded box rather than spot-checked, since both failure modes live in cells nobody would
/// think to pick, and over every HOST, or the Wood and Sand passes go unexecuted.
#[test]
fn the_reveal_stamp_and_the_dig_path_agree() {
	use crate::worldgen::nodes::{self, Levels};
	let seed = 42.0;
	let span = PADDED as i32 - 1;
	let max = crate::skills::MAX_LEVEL;
	// Every host a node can manifest in, each filling a whole box.
	let hosts = [B::Stone, B::Wood, B::Sand];
	// Which hosts the sweep actually STAMPED something in. An agreement test over boxes that
	// hold none is green for the wrong reason. Per host, and that is the whole point: one bool
	// OR-ed across all three lets the springstone box satisfy it alone while the Wood pass
	// stamps nothing anywhere in the sweep.
	let mut stamped_host = std::collections::HashSet::new();
	// Boxes straddling the bands' edges, both reveal axes stepped —
	// mixed tiers, edited cells, and the leystone band's seam with the
	// deep tiers all in one sweep.
	for (origin, mining, alchemy, edited) in [
		((-24, 1, -4), max, max, vec![]),
		((-24, 1, -4), 10, 1, vec![]),
		(
			(-24, 1, -4),
			30,
			7,
			vec![pidx(0, 0, 0) as u16, pidx(3, 4, 5) as u16],
		),
		((0, 100, -16), max, max, vec![]),
		((0, 100, -16), max, 1, vec![]),
		((0, 120, -16), 1, max, vec![]),
		((0, 40, -24), 55, 3, vec![]),
		// A box holding real SPRINGSTONE beds — found by walking the
		// seed, and asserted below to still hold one, because a
		// coordinate that quietly stopped covering the Sand pass is
		// exactly how the snapshot prefilter survived this test.
		((8, 146, -400), max, max, vec![]),
		((8, 146, -400), max, 11, vec![]),
		// And one holding a real HEARTWOOD trunk, for the same
		// reason and found the same way. Without it the Wood pass
		// stamped nothing anywhere in this sweep, so every case
		// agreed trivially at "neither side sees a node" and the
		// pass was untested by the test that claims to cover it.
		// The second entry is the day-one reveal: heartwood is
		// visible at the level everyone starts at.
		((-481, 127, -481), max, max, vec![]),
		((-481, 127, -481), 1, 1, vec![]),
		// Below every reveal on both axes nothing may be stamped.
		((-24, 1, -4), 9, 1, vec![]),
	] {
		for host in hosts {
			let mut input = MeshInput {
				blocks: Box::new([host as u16; PADDED_VOLUME]),
				missing: [false; 6],
				nodes: Some(NodeStamp {
					seed,
					origin,
					levels: Levels { mining, alchemy },
					hosts: hosts_of(host),
					edited: edited.clone(),
				}),
			};
			input.apply_nodes();
			if input.blocks.iter().any(|&c| c != host as u16) {
				stamped_host.insert(host as u16);
			}
			let levels = Levels { mining, alchemy };
			for ly in 0..=span {
				for lz in 0..=span {
					for lx in 0..=span {
						let i = ((ly * PADDED as i32 + lz) * PADDED as i32 + lx) as usize;
						let (x, y, z) = (origin.0 + lx, origin.1 + ly, origin.2 + lz);
						let was_edited = edited.contains(&(i as u16));
						let want =
							nodes::revealed(host as u16, (x, y, z), seed, levels, was_edited)
								.map_or(host as u16, |o| o as u16);
						assert_eq!(
							input.blocks[i], want,
							"{host:?} ({x},{y},{z}) at mining {mining} / alchemy {alchemy}"
						);
					}
				}
			}
		}
	}
	for host in hosts {
		assert!(
			stamped_host.contains(&(host as u16)),
			"the sweep stamped nothing in {host:?} — that pass is not \
				 being exercised, so this test proves nothing about it"
		);
	}
}

/// The stamp only ever touches PRISTINE stone: air stays air, a spent rock somebody has built
/// with stays exactly what it is, and a shaped stone cell is not stone. **Swept across Mining
/// levels**, the half that matters: at the cap the second pass reverts nothing, so a run there
/// cannot tell "only claimed cells are reverted" from "everything is". The case is a visitor at
/// Mining 1 in a friend's hall of tapped Aetherock, which a sweeping stamp would turn to stone.
#[test]
fn the_reveal_stamp_leaves_everything_else_alone() {
	use crate::worldgen::{nodes::Levels, ore};
	let seed = 42.0;
	let origin = (-24, 1, -4);
	let others = [
		B::Air as u16,
		B::DepletedEmberore as u16,
		B::DepletedAetherock as u16,
		B::DepletedStone as u16,
		B::Rubble as u16,
		crate::chunk::make_cell(B::StoneSlab, 1),
	];
	// Every level a reveal happens at, either side, plus both ends of
	// the ladder — the thresholds are where a sweep-vs-claims bug
	// would first show.
	let mut levels = vec![1, crate::skills::MAX_LEVEL];
	for &o in &ore::ORES {
		let r = crate::unlock::reveal(o).expect("an ore");
		levels.extend([r.saturating_sub(1).max(1), r]);
	}
	for mining in levels {
		let mut input = MeshInput {
			blocks: Box::new([B::Stone as u16; PADDED_VOLUME]),
			missing: [false; 6],
			nodes: Some(NodeStamp {
				seed,
				origin,
				levels: Levels {
					mining,
					alchemy: crate::skills::MAX_LEVEL,
				},
				hosts: hosts_of(B::Stone),
				edited: Vec::new(),
			}),
		};
		// Salt the array so every non-stone value sits on a cell the
		// stamp would otherwise have claimed.
		let veins: Vec<usize> = (0..PADDED_VOLUME)
			.filter(|&i| {
				let (lx, lz, ly) = (i % PADDED, (i / PADDED) % PADDED, i / (PADDED * PADDED));
				ore::ore_at(
					origin.0 + lx as i32,
					origin.1 + ly as i32,
					origin.2 + lz as i32,
					seed,
				)
				.is_some()
			})
			.collect();
		assert!(veins.len() >= others.len(), "need vein cells to salt");
		for (slot, &v) in veins.iter().zip(others.iter()) {
			input.blocks[*slot] = v;
		}
		input.apply_nodes();
		for (slot, &v) in veins.iter().zip(others.iter()) {
			assert_eq!(
				input.blocks[*slot], v,
				"the stamp rewrote a non-stone cell at mining {mining}"
			);
		}
	}
}

#[test]
fn lone_dirt_has_six_faces() {
	let m = mesh_chunk(&isolated(&solo(B::Dirt)));
	assert_eq!(m.solid.len(), 6 * 8);
	assert!(m.water.is_empty());
}

/// State bits (the reserved nibble above the block id) must not
/// change meshing: MeshInput::build strips them, so a stateful cell
/// meshes exactly like its plain block.
#[test]
fn state_bits_on_unshaped_blocks_do_not_change_meshing() {
	// The masking discipline: for blocks WITHOUT a shape, a state
	// nibble must be invisible to the mesher (get() masks it).
	let mut plain = ChunkData::default();
	plain.set(8, 8, 8, B::Dirt);
	plain.set(8, 9, 8, B::Water);
	let mut stateful = ChunkData::default();
	stateful.set_cell(
		8,
		8,
		8,
		B::Dirt as u16 | 0b0101 << crate::chunk::STATE_SHIFT,
	);
	stateful.set_cell(
		8,
		9,
		8,
		B::Water as u16 | 0b1111 << crate::chunk::STATE_SHIFT,
	);
	let a = mesh_chunk(&isolated(&plain));
	let b = mesh_chunk(&isolated(&stateful));
	assert_eq!(a.solid, b.solid, "solid stream byte-identical");
	assert_eq!(a.water, b.water, "water stream byte-identical");
	assert_eq!(a.impacts, b.impacts);
}

/// A stair at each state, meshed solo in the middle of an otherwise
/// empty chunk (world origin ZERO).
fn stair_mesh(facing: crate::shapes::Facing) -> MeshData {
	let mut d = ChunkData::default();
	d.set_cell(
		8,
		8,
		8,
		crate::chunk::make_cell(B::WoodStairs, crate::shapes::stair_state(facing)),
	);
	mesh_chunk(&isolated(&d))
}

#[test]
fn shaped_state_changes_geometry_and_rotations_are_permutations() {
	use crate::shapes::Facing;
	let a = stair_mesh(Facing::PosX);
	let b = stair_mesh(Facing::NegX);
	assert_ne!(a.solid, b.solid, "facing changes the mesh");
	// Same quad count and the same multiset of (texture, uv) payloads
	// — a rotation permutes geometry, never invents or drops faces.
	assert_eq!(a.solid.len(), b.solid.len());
	let key = |m: &MeshData| {
		let mut texes: Vec<u32> = m.solid.chunks(2).map(|p| p[1] & 0x3ffff).collect();
		texes.sort_unstable();
		texes
	};
	assert_eq!(key(&a), key(&b), "tex/uv payloads are a permutation");
}

#[test]
fn shaped_boundary_culling() {
	use crate::chunk::make_cell;
	use crate::shapes::{Facing, stair_state};
	// Stair ascending +x with an opaque block at +x: the stair's full
	// back face is culled, and the wall's −x face is culled too (the
	// stair's back fully covers the shared plane).
	let mut d = ChunkData::default();
	d.set_cell(8, 8, 8, make_cell(B::WoodStairs, stair_state(Facing::PosX)));
	d.set(9, 8, 8, B::Stone);
	let m = mesh_chunk(&isolated(&d));
	let stair_back = m
		.solid
		.chunks(8)
		.any(|q| dir_of(q[0]) == Dir::West as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 9 * 12));
	assert!(!stair_back, "stair back culled against the wall");
	let wall_west_face = m
		.solid
		.chunks(8)
		.any(|q| dir_of(q[0]) == Dir::East as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 9 * 12));
	assert!(!wall_west_face, "wall face culled behind the stair back");

	// A block ABOVE the stair keeps its bottom face (the stair's top
	// is partial), and the stair keeps its treads.
	let mut d2 = ChunkData::default();
	d2.set_cell(8, 8, 8, make_cell(B::WoodStairs, stair_state(Facing::PosX)));
	d2.set(8, 9, 8, B::Stone);
	let m2 = mesh_chunk(&isolated(&d2));
	let above_bottom = m2.solid.chunks(8).any(|q| {
		dir_of(q[0]) == Dir::Bottom as u32 && q.chunks(2).all(|p| ((p[0] >> 8) & 0xff) == 108)
	});
	assert!(above_bottom, "block above a stair keeps its bottom face");

	// Two stairs stacked mouth-to-mouth (lower ascends +x, upper is
	// the same shape upside-down is impossible — instead: two stairs
	// side by side facing each other cull their touching full backs).
	let mut d3 = ChunkData::default();
	d3.set_cell(8, 8, 8, make_cell(B::WoodStairs, stair_state(Facing::PosX)));
	d3.set_cell(9, 8, 8, make_cell(B::WoodStairs, stair_state(Facing::NegX)));
	let m3 = mesh_chunk(&isolated(&d3));
	let touching_backs = m3.solid.chunks(8).any(|q| {
		let plane = q.chunks(2).all(|p| (p[0] & 0xff) == 9 * 12);
		plane && (dir_of(q[0]) == Dir::West as u32 || dir_of(q[0]) == Dir::East as u32)
	});
	assert!(!touching_backs, "mirrored stairs cull their shared plane");
}

#[test]
fn shaped_culling_works_across_chunk_borders() {
	use crate::chunk::make_cell;
	use crate::shapes::{Facing, stair_state};
	// Stair at the chunk's +x border, opaque neighbor chunk beyond:
	// the padded shell carries the neighbor, and the stair's full
	// back face is culled through it.
	let mut d = ChunkData::default();
	d.set_cell(
		15,
		8,
		8,
		make_cell(B::WoodStairs, stair_state(Facing::PosX)),
	);
	let mut wall = ChunkData::default();
	for y in 0..16 {
		for z in 0..16 {
			wall.set(0, y, z, B::Stone);
		}
	}
	let input = MeshInput::build(
		&d,
		|dx, _, _| {
			if dx == 1 { Src::Chunk(&wall) } else { Src::Air }
		},
	);
	let m = mesh_chunk(&input);
	let back = m
		.solid
		.chunks(8)
		.any(|q| dir_of(q[0]) == Dir::West as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 16 * 12));
	assert!(!back, "cross-chunk cull via the state-carrying shell");
}

#[test]
fn water_beside_shaped_emits_no_interface_face() {
	// The pinned v1 artifact: water's Translucent rule emits only
	// toward class-None, so no face appears against a stair — you
	// can see through the stair's open region into the water void.
	let mut d = ChunkData::default();
	d.set_cell(
		8,
		8,
		8,
		crate::chunk::make_cell(B::WoodStairs, crate::shapes::stair_state(Facing::PosX)),
	);
	d.set(7, 8, 8, B::Water);
	use crate::shapes::Facing;
	let m = mesh_chunk(&isolated(&d));
	let interface = m
		.water
		.chunks(8)
		.any(|q| dir_of(q[0]) == Dir::West as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 8 * 12));
	assert!(!interface, "no water face against the shaped cell");
}

#[test]
fn water_only_faces_air() {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Water);
	d.set(8, 7, 8, B::Dirt); // below
	let m = mesh_chunk(&isolated(&d));
	// Water faces air on every open side. The dirt below keeps every
	// face, its top included: that face borders water, and water is
	// not solid, so the dirt top IS visible through it.
	let water_faces = m.water.len() / 8; // two words per water vertex
	assert_eq!(water_faces, 5, "water faces air on 4 sides + top");
	let solid_faces = m.solid.len() / 8;
	assert_eq!(solid_faces, 6, "dirt visible through water above");
}

#[test]
fn water_water_interface_emits_nothing() {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Water);
	d.set(9, 8, 8, B::Water);
	let m = mesh_chunk(&isolated(&d));
	// Each water block: faces toward air except toward each other.
	assert_eq!(m.water.len() / 8, 2 * 5);
}

#[test]
fn missing_neighbor_suppresses_border_faces() {
	let mut d = ChunkData::default();
	d.set(0, 8, 8, B::Dirt); // on the −x border
	let m = mesh_chunk(&isolated(&d));
	assert_eq!(
		m.solid.len() / 8,
		5,
		"border face toward missing neighbor suppressed"
	);
}

#[test]
fn air_sentinel_above_world_top() {
	let mut d = ChunkData::default();
	d.set(8, 15, 8, B::Dirt);
	let input = MeshInput::build(&d, |dx, dy, dz| {
		if (dx, dy, dz) == (0, 1, 0) {
			Src::Air
		} else {
			Src::Missing
		}
	});
	let m = mesh_chunk(&input);
	assert_eq!(m.solid.len() / 8, 6, "top face vs air sentinel");
}

#[test]
fn neighbor_chunk_occludes() {
	let mut d = ChunkData::default();
	d.set(15, 8, 8, B::Dirt); // on the +x border
	let mut other = ChunkData::default();
	other.set(0, 8, 8, B::Dirt); // the neighbor's facing block
	let input = MeshInput::build(&d, |dx, dy, dz| {
		if (dx, dy, dz) == (1, 0, 0) {
			Src::Chunk(&other)
		} else {
			Src::Missing
		}
	});
	let m = mesh_chunk(&input);
	assert_eq!(m.solid.len() / 8, 5, "+x face occluded by neighbor block");
}

#[test]
fn shell_regions_land_at_padded_coords() {
	// One marked block per source kind: a face neighbor (below), an
	// edge neighbor (+x+z), and a corner neighbor (+x+y+z).
	let mut below = ChunkData::default();
	below.set(5, 15, 7, B::Stone); // → the shell layer underneath
	let mut edge = ChunkData::default();
	edge.set(0, 8, 0, B::Dirt); // → the +x+z shell edge
	let mut corner = ChunkData::default();
	corner.set(0, 0, 0, B::Sand); // → the +x+y+z shell corner
	let input = MeshInput::build(&ChunkData::default(), |dx, dy, dz| match (dx, dy, dz) {
		(0, -1, 0) => Src::Chunk(&below),
		(1, 0, 1) => Src::Chunk(&edge),
		(1, 1, 1) => Src::Chunk(&corner),
		_ => Src::Air,
	});
	assert_eq!(input.get(5, -1, 7), B::Stone as u16);
	assert_eq!(input.get(16, 8, 16), B::Dirt as u16);
	assert_eq!(input.get(16, 16, 16), B::Sand as u16);
	// Everything else in the shell is air.
	assert_eq!(input.get(-1, -1, -1), 0);
	assert_eq!(input.get(8, 8, 8), 0, "empty center");
}

#[test]
fn missing_face_sets_flag_missing_diagonal_does_not() {
	let input = MeshInput::build(&ChunkData::default(), |dx, dy, dz| {
		// Top face missing, all edges/corners missing, other faces Air.
		if (dx, dy, dz) == (0, 1, 0) || dx.abs() + dy.abs() + dz.abs() > 1 {
			Src::Missing
		} else {
			Src::Air
		}
	});
	let mut expect = [false; 6];
	expect[Dir::Top as usize] = true;
	assert_eq!(input.missing, expect);
}

#[test]
fn packed_vertex_roundtrip() {
	// A high layer index proves the widened field (the old format
	// capped far lower).
	let w0 = pack((16 * 12, 7 * 12, 3 * 12), Dir::East, 2);
	assert_eq!(x_of(w0), 16);
	assert_eq!(y_of(w0), 7);
	assert_eq!(z_of(w0), 3);
	assert_eq!(
		w0 & 0xff,
		192,
		"positions are stored in twelfth-block units"
	);
	assert_eq!(dir_of(w0), Dir::East as u32);
	assert_eq!(corner_of(w0), 2);
	assert_eq!(w0 >> 29, 0, "bits 29-31 are free after the repack");
	let w1 = solid_word1(700, 12, 0, false, false);
	assert_eq!(w1 & 0x3ff, 700);
	assert_eq!((w1 >> 10) & 0xf, 12, "uv u in twelfths");
	assert_eq!((w1 >> 14) & 0xf, 0, "uv v in twelfths");
	assert_eq!((w1 >> 18) & 1, 0, "emissive bit clear");
	assert_eq!((w1 >> 19) & 1, 0, "tint-from-below bit clear");
	assert_eq!(w1 >> 20, 0, "bits 20-31 are free after the repack");
	let lit = solid_word1(700, 12, 0, true, false);
	assert_eq!((lit >> 18) & 1, 1, "emissive bit set");
	let down = solid_word1(700, 12, 0, false, true);
	assert_eq!((down >> 19) & 1, 1, "tint-from-below bit set");
}

/// The mesh is where a door's two halves agree to wear one tint: the upper half's every vertex
/// carries word 1's tint-from-below flag, so chunk.wgsl hashes the cell below it and both halves
/// land on the same tint. The lower half must NOT carry it, or the leaf tints off the block under
/// the door.
#[test]
fn a_doors_upper_half_tints_from_the_cell_below() {
	use crate::shapes::{Facing, door_state};
	let tint_bits = |upper: bool| {
		let mut d = ChunkData::default();
		d.set_cell(
			8,
			8,
			8,
			crate::chunk::make_cell(B::LumberDoor, door_state(Facing::PosX, false, upper)),
		);
		let m = mesh_chunk(&isolated(&d));
		assert!(!m.solid.is_empty(), "a door half meshes to something");
		m.solid
			.chunks_exact(2)
			.map(|w| (w[1] >> 19) & 1)
			.collect::<std::collections::HashSet<u32>>()
	};
	assert_eq!(
		tint_bits(false),
		std::collections::HashSet::from([0]),
		"the lower half hashes its own cell"
	);
	assert_eq!(
		tint_bits(true),
		std::collections::HashSet::from([1]),
		"the upper half hashes the one below"
	);
}

// Decode helpers — the mirror of `pack`/`solid_word1`. Positions come
// back in whole blocks, since full-block geometry lands on the
// twelfths grid's whole-block lattice points.
fn x_of(w0: u32) -> u32 {
	(w0 & 0xff) / 12
}

fn y_of(w0: u32) -> u32 {
	((w0 >> 8) & 0xff) / 12
}

fn z_of(w0: u32) -> u32 {
	((w0 >> 16) & 0xff) / 12
}

fn dir_of(w0: u32) -> u32 {
	(w0 >> 24) & 0x7
}

/// The (word0, word1) vertex pairs of a water face, matched like
/// `quad_of` but over the interleaved two-word stream.
fn water_quad(m: &[u32], bx: i32, by: i32, bz: i32, dir: Dir) -> [(u32, u32); 4] {
	let want: std::collections::HashSet<(u32, u32, u32)> = face_corners(bx, by, bz, dir)
		.iter()
		.map(|&(x, y, z)| (x as u32, y as u32, z as u32))
		.collect();
	for q in m.chunks(8) {
		let got: std::collections::HashSet<(u32, u32, u32)> = q
			.chunks(2)
			.map(|p| (x_of(p[0]), y_of(p[0]), z_of(p[0])))
			.collect();
		if dir_of(q[0]) == dir as u32 && got == want {
			return [(q[0], q[1]), (q[2], q[3]), (q[4], q[5]), (q[6], q[7])];
		}
	}
	panic!("water face of ({bx},{by},{bz}) toward {dir:?} not found");
}

fn drop_of(w1: u32) -> u32 {
	(w1 >> 10) & 0xf
}

fn falling_of(w1: u32) -> bool {
	(w1 >> 14) & 1 == 1
}

#[test]
fn source_surface_drops_one_ninth() {
	// Lone source with air above: a source is the strongest amount
	// there is, so its top face sits one code under the cell top all
	// the way across; side faces drop only their top-plane vertices;
	// nothing is falling.
	let m = mesh_chunk(&isolated(&solo(B::Water)));
	let top = water_quad(&m.water, 8, 8, 8, Dir::Top);
	for (_, w1) in top {
		assert_eq!(drop_of(w1), 1);
		assert!(!falling_of(w1));
	}
	let side = water_quad(&m.water, 8, 8, 8, Dir::West);
	for (w0, w1) in side {
		assert_eq!(
			drop_of(w1),
			u32::from(y_of(w0) == 9),
			"top verts drop, bottom full"
		);
	}
}

#[test]
fn submerged_water_renders_full_cube() {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Water);
	d.set(8, 9, 8, B::Water);
	let m = mesh_chunk(&isolated(&d));
	// Lower cell: water above → its side faces stay full height.
	let side = water_quad(&m.water, 8, 8, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!(drop_of(w1), 0);
	}
	// Upper cell is the surface: top face drops.
	let top = water_quad(&m.water, 8, 9, 8, Dir::Top);
	for (_, w1) in top {
		assert_eq!(drop_of(w1), 1);
	}
}

#[test]
fn water_under_ceiling_renders_full_cube() {
	// Water pressed against an opaque block above (cave ceiling, the
	// wall over a dug hole): no open surface exists, so the sides
	// stay full height. Dropping them opened a see-through gap under
	// the ceiling that nothing else covered.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Water);
	d.set(8, 9, 8, B::Stone);
	let m = mesh_chunk(&isolated(&d));
	// 4 sides + bottom; the top face is occluded by the stone.
	assert_eq!(m.water.len() / 8, 5);
	for pair in m.water.chunks(2) {
		assert_eq!(drop_of(pair[1]), 0, "covered cell stays full");
	}
}

#[test]
fn covered_inflow_cell_never_vanishes() {
	// The flooded-cave regression: water under an opaque ceiling beside an air column with
	// water directly below it, as when water pours through a dug hole into a cave. The corner
	// plunge collapses every side face to zero area while the ceiling occludes the top, so the
	// cell renders NOTHING and you see straight through into the water behind.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFall);
	d.set(8, 9, 8, B::Stone);
	d.set(9, 7, 8, B::Water); // plunge trigger in the +x column
	let m = mesh_chunk(&isolated(&d));
	let side = water_quad(&m.water, 8, 8, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!(drop_of(w1), 0, "sealed at full height");
	}
}

#[test]
fn open_surface_pins_full_against_covered_neighbor() {
	// Open surface water beside a covered cell (water running out
	// from under an overhang): the shared corners pin full so the
	// surface rises to meet the covered cell's full-height cube
	// instead of dipping into a see-through sliver at the lip.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Water);
	d.set(8, 9, 8, B::Stone);
	d.set(7, 8, 8, B::Water); // open: air above
	let m = mesh_chunk(&isolated(&d));
	let top = water_quad(&m.water, 7, 8, 8, Dir::Top);
	for (w0, w1) in top {
		let expect = u32::from(x_of(w0) != 8);
		assert_eq!(drop_of(w1), expect, "shared corners pin, outer dip");
	}
}

#[test]
fn waterfall_top_drops_like_a_surface_cell() {
	let m = mesh_chunk(&isolated(&solo(B::WaterFall)));
	// Floating fall: 4 sides + top + bottom toward air. Air above, so
	// its top-plane vertices take the corner surface like any surface
	// water — with no feeder in reach it keeps its nominal amount —
	// and everything is flagged falling.
	assert_eq!(m.water.len() / 8, 6);
	for pair in m.water.chunks(2) {
		assert_eq!(drop_of(pair[1]), u32::from(y_of(pair[0]) == 9));
		assert!(falling_of(pair[1]));
	}
	// Mid-column (falling water above): a complete cube, no drop.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFall);
	d.set(8, 9, 8, B::WaterFall);
	let m = mesh_chunk(&isolated(&d));
	let side = water_quad(&m.water, 8, 8, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!(drop_of(w1), 0, "submerged fall stays full");
	}
}

#[test]
fn cascade_steps_ramp_into_each_other() {
	// The fluid sim's staircase shape: flow on the upper step, flow one down and one across
	// on the lower step. The upper surface's downhill corners plunge the whole block and the
	// lower cell's uphill corners pin full, so the two surfaces meet exactly at the lattice —
	// a connected ramp, no cliff sliver between.
	let mut d = ChunkData::default();
	d.set(8, 9, 8, B::WaterFlow7);
	d.set(8, 8, 8, B::Stone);
	d.set(9, 8, 8, B::WaterFlow7);
	d.set(9, 7, 8, B::Stone);
	let m = mesh_chunk(&isolated(&d));
	let upper = water_quad(&m.water, 8, 9, 8, Dir::Top);
	for (w0, w1) in upper {
		let expect = if x_of(w0) == 9 { 9 } else { 2 };
		assert_eq!(drop_of(w1), expect, "downhill edge plunges");
	}
	let lower = water_quad(&m.water, 9, 8, 8, Dir::Top);
	for (w0, w1) in lower {
		let expect = if x_of(w0) == 9 { 0 } else { 2 };
		assert_eq!(drop_of(w1), expect, "uphill edge pins full");
	}
	// The upper cell's downhill side face collapsed to zero area and
	// is skipped: upper emits N/S/E/Top, lower W/N/S/Top (E faces
	// the stone step, bottoms sit on stone).
	assert_eq!(m.water.len() / 8, 4 + 4);
}

#[test]
fn waterfall_lip_knits_with_the_feeding_pool() {
	// Top fall block beside the upper pool's surface source — the worldgen lip arrangement.
	// The interface face is culled, so the two top surfaces must agree on their shared corners
	// or the rim is a see-through crack. The fall surfaces one level below its feeder, so the
	// shared corners hold the pool's own height while the lip's outer edge dips one code below
	// it — the surface tips over the edge instead of popping back up to full height.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFall);
	d.set(7, 8, 8, B::Water);
	let m = mesh_chunk(&isolated(&d));
	let pool_top = water_quad(&m.water, 7, 8, 8, Dir::Top);
	for (_, w1) in pool_top {
		assert_eq!(drop_of(w1), 1, "pool surface undisturbed");
	}
	let fall_top = water_quad(&m.water, 8, 8, 8, Dir::Top);
	for (w0, w1) in fall_top {
		let expect = if x_of(w0) == 9 { 2 } else { 1 };
		assert_eq!(drop_of(w1), expect, "shared corners knit, outer dip");
	}
}

/// A source poured against a wall face, with nothing solid under it: the sim sends it straight
/// down and only the landing spreads, so the placed cell is ringed by air at its own level with
/// the pool one below — the plunge trigger, on all four corners at once. The source must still
/// render as a full block; a plunge outranking the source pin zeroes the top face, skips every
/// side face as zero-area, and the block you just placed renders nothing at all.
#[test]
fn a_spilling_source_keeps_its_full_height() {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Water);
	d.set(8, 7, 8, B::WaterFall); // pouring straight down...
	for (x, z) in [
		(9, 7),
		(7, 7),
		(9, 8),
		(7, 8),
		(9, 9),
		(7, 9),
		(8, 7),
		(8, 9),
	] {
		d.set(x, 7, z, B::WaterFlow4); // ...and spreading below
	}
	let m = mesh_chunk(&isolated(&d));
	for (_, w1) in water_quad(&m.water, 8, 8, 8, Dir::Top) {
		assert_eq!(drop_of(w1), 1, "the source stays 8/9 tall");
	}
	// Every side survives too — `water_quad` panics on a missing
	// face, and a collapsed top edge is exactly what used to make
	// `emit_water_face` skip these as zero-area.
	for dir in [Dir::West, Dir::East, Dir::North, Dir::South] {
		for (w0, w1) in water_quad(&m.water, 8, 8, 8, dir) {
			assert_eq!(drop_of(w1), u32::from(y_of(w0) == 9), "{dir:?} side full");
		}
	}
}

#[test]
fn fall_lip_steps_below_its_feeder() {
	// A thin flow feeding a fall: the lip continues the downhill
	// slope one level below the feeder.
	let mut d = ChunkData::default();
	d.set(7, 8, 8, B::WaterFlow5);
	d.set(8, 8, 8, B::WaterFall);
	let m = mesh_chunk(&isolated(&d));
	let fall_top = water_quad(&m.water, 8, 8, 8, Dir::Top);
	for (w0, w1) in fall_top {
		let expect = if x_of(w0) == 9 { 5 } else { 4 };
		assert_eq!(drop_of(w1), expect, "lip = feeder − 1");
	}
}

#[test]
fn flow_direction_points_at_the_emptiest_side() {
	// Flow5 walled in by water except +x: flows toward +x.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFlow5);
	d.set(7, 8, 8, B::WaterFlow4);
	d.set(8, 8, 9, B::WaterFlow4);
	d.set(8, 8, 7, B::WaterFlow4);
	let m = mesh_chunk(&isolated(&d));
	let top = water_quad(&m.water, 8, 8, 8, Dir::Top);
	for (_, w1) in top {
		assert_eq!((w1 >> 15) & 7, 1, "flows toward +x");
	}
	// Sources rest even with open sides.
	let m = mesh_chunk(&isolated(&solo(B::Water)));
	let top = water_quad(&m.water, 8, 8, 8, Dir::Top);
	for (_, w1) in top {
		assert_eq!((w1 >> 15) & 7, 0, "sources are still");
	}
}

#[test]
fn landed_fall_carries_the_foam_bit() {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFall);
	d.set(8, 7, 8, B::Stone);
	let m = mesh_chunk(&isolated(&d));
	let side = water_quad(&m.water, 8, 8, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!((w1 >> 18) & 1, 1, "splash cell foams");
	}
	// A fall still in the air doesn't.
	let m = mesh_chunk(&isolated(&solo(B::WaterFall)));
	let side = water_quad(&m.water, 8, 8, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!((w1 >> 18) & 1, 0, "mid-air fall has no foam");
	}
}

fn fall_of(w1: u32) -> u32 {
	(w1 >> 20) & 0xf
}

fn splash_of(w1: u32) -> u32 {
	(w1 >> 18) & 3
}

#[test]
fn impact_cells_ride_mesh_data() {
	// A landed fall records its cell in MeshData.impacts once (not
	// per face); mid-air falls record nothing.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFall);
	d.set(8, 7, 8, B::Stone);
	d.set(10, 8, 8, B::WaterFall);
	d.set(10, 7, 8, B::Water);
	let m = mesh_chunk(&isolated(&d));
	let mut impacts = m.impacts.clone();
	impacts.sort_unstable();
	assert_eq!(impacts, vec![8 | 8 << 4 | 8 << 8, 10 | 8 << 4 | 8 << 8]);
	let m = mesh_chunk(&isolated(&solo(B::WaterFall)));
	assert!(m.impacts.is_empty(), "mid-air fall is not an emitter");
}

#[test]
fn a_fall_landing_in_its_own_pool_is_still_an_emitter() {
	// The shape a player-built waterfall actually makes: the fall punches to the floor and the
	// pool it spreads closes around it, so the impact cell is capped by the fall above and
	// ringed by water — EVERY face culled. Recording the impact must not hang off emitting a
	// face, or there is no spray and no waterfall audio, while worldgen falls (landing in open
	// air one cell above a pool) still work and hide it.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFall);
	d.set(8, 9, 8, B::WaterFall);
	d.set(8, 7, 8, B::Stone);
	for (x, z) in [(7, 8), (9, 8), (8, 7), (8, 9)] {
		d.set(x, 8, z, B::WaterFlow4); // the pool, sealing every side
		d.set(x, 7, z, B::Stone);
	}
	let m = mesh_chunk(&isolated(&d));
	assert!(
		!m.water.chunks(8).any(|q| {
			let c = |i: usize| (x_of(q[i * 2]), y_of(q[i * 2]), z_of(q[i * 2]));
			(0..4).all(|i| {
				let (cx, cy, cz) = c(i);
				(8..=9).contains(&cx) && (8..=9).contains(&cy) && (8..=9).contains(&cz)
			})
		}),
		"the impact cell really is fully enclosed; the test proves nothing otherwise"
	);
	assert_eq!(
		m.impacts,
		vec![8 | 8 << 4 | 8 << 8],
		"impact still recorded"
	);
}

#[test]
fn worldgen_style_pool_landing_foams() {
	// A fall ending on resting water (the worldgen arrangement —
	// falls land in pools, not on stone) is an impact cell. The old
	// foam predicate required an opaque block below and never fired
	// for these.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFall);
	d.set(8, 7, 8, B::Water);
	let m = mesh_chunk(&isolated(&d));
	let side = water_quad(&m.water, 8, 8, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!(splash_of(w1), 3, "pool landing is an impact");
	}
}

#[test]
fn pool_cells_get_splash_rings() {
	// Fall into the middle of a pool wide enough to hold the whole
	// ring: surface cells score by Chebyshev distance out from the
	// impact, one code lower per ring and nothing past the scan's
	// reach. Mid-fall cells above the impact stay ring-free.
	let mut d = ChunkData::default();
	for z in 5..=11 {
		for x in 5..=11 {
			d.set(x, 7, z, B::Water);
		}
	}
	d.set(8, 8, 8, B::WaterFall);
	d.set(8, 9, 8, B::WaterFall);
	let m = mesh_chunk(&isolated(&d));
	for (dist, expect) in [(1, 2), (2, 1), (3, 0)] {
		let top = water_quad(&m.water, 8 + dist, 7, 8, Dir::Top);
		for (_, w1) in top {
			assert_eq!(splash_of(w1), expect, "ring at distance {dist}");
		}
	}
	let mid_fall = water_quad(&m.water, 8, 9, 8, Dir::West);
	for (_, w1) in mid_fall {
		assert_eq!(splash_of(w1), 0, "mid-air sheet cell has no splash");
	}
}

/// A coastal/ocean-shaped chunk: a chunk-wide two-deep pool on a
/// stone floor, two waterfalls landing in it, a flowing rim, pillars
/// breaking the surface, and water continuing across the +x seam.
/// Every open surface cell here reaches the splash ring scan.
fn water_heavy() -> MeshData {
	let mut d = ChunkData::default();
	for z in 0..16 {
		for x in 0..16 {
			d.set(x, 5, z, B::Stone);
			d.set(x, 6, z, B::Water);
			d.set(x, 7, z, B::Water);
		}
	}
	for y in 8..=13 {
		d.set(4, y, 4, B::WaterFall);
	}
	for y in 8..=10 {
		d.set(11, y, 11, B::WaterFall);
	}
	// Flowing rim on the −z edge, thinning across x.
	for x in 0..16 {
		d.set(x, 8, 0, B::water_flow(1 + (x % 7) as u8));
	}
	d.set(2, 8, 9, B::Stone);
	d.set(9, 8, 2, B::Stone);
	let mut east = ChunkData::default();
	for z in 0..16 {
		for y in 6..=7 {
			east.set(0, y, z, B::Water);
		}
	}
	let input = MeshInput::build(&d, |dx, dy, dz| {
		if (dx, dy, dz) == (1, 0, 0) {
			Src::Chunk(&east)
		} else {
			Src::Air
		}
	});
	mesh_chunk(&input)
}

/// FNV-1a over every emitted word — a whole-mesh byte pin.
fn digest(m: &MeshData) -> (usize, usize, usize, u64) {
	let mut h = 0xcbf2_9ce4_8422_2325u64;
	for w in m
		.solid
		.iter()
		.chain(&m.water)
		.copied()
		.chain(m.impacts.iter().map(|&i| u32::from(i)))
	{
		h ^= u64::from(w);
		h = h.wrapping_mul(0x0000_0100_0000_01b3);
	}
	(m.solid.len(), m.water.len(), m.impacts.len(), h)
}

/// The splash ring scan's fast path (skipping the neighborhood walk when no layer in reach holds
/// falling water) must leave every stream byte-identical — the impacts list drives spray emitters
/// AND waterfall audio, so a miss is silent in the mesh and audible in the world. Quad COUNTS are
/// the part that matters: a count that moves means a culling or emission change and must not be
/// re-pinned without finding out why.
#[test]
fn water_heavy_chunk_mesh_is_pinned() {
	let m = water_heavy();
	assert!(!m.impacts.is_empty(), "fixture has landing falls");
	assert!(
		m.water.chunks(2).any(|p| splash_of(p[1]) == 2),
		"fixture has ring cells the scan must still find"
	);
	assert_eq!(
		digest(&m),
		(4704, 3224, 2, 0x89f2_6ce6_0311_3828),
		"water-heavy mesh + impacts byte-identical"
	);
}

#[test]
fn fall_distance_counts_from_lip() {
	// A fall column several cells tall: each cell counts the falling
	// cells above it; side-face top-plane verts carry that count, and
	// bottom-plane
	// verts one more, so interpolation is continuous down the sheet.
	let mut d = ChunkData::default();
	for y in 8..=12 {
		d.set(8, y, 8, B::WaterFall);
	}
	let m = mesh_chunk(&isolated(&d));
	for (cell_y, expect) in [(12, 0), (8, 4)] {
		let side = water_quad(&m.water, 8, cell_y, 8, Dir::West);
		for (w0, w1) in side {
			let top_plane = y_of(w0) == cell_y as u32 + 1;
			assert_eq!(fall_of(w1), expect + u32::from(!top_plane));
		}
	}
	// Non-falling water carries zero fall bits.
	let m = mesh_chunk(&isolated(&solo(B::Water)));
	for pair in m.water.chunks(2) {
		assert_eq!(fall_of(pair[1]), 0);
	}
}

#[test]
fn fall_distance_saturates() {
	// A full-height column: the deepest cells saturate instead of
	// wrapping the fall-distance field.
	let mut d = ChunkData::default();
	for y in 0..16 {
		d.set(8, y, 8, B::WaterFall);
	}
	let m = mesh_chunk(&isolated(&d));
	let side = water_quad(&m.water, 8, 0, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!(fall_of(w1), 15, "deep cells saturate");
	}
	// A fall entering through the top shell (still falling above the
	// snapshot ceiling) saturates too — the partial count would reset
	// the sheet to a glassy lip at every vertical chunk seam.
	let mut d = ChunkData::default();
	d.set(8, 15, 8, B::WaterFall);
	let mut above = ChunkData::default();
	above.set(8, 0, 8, B::WaterFall);
	let input = MeshInput::build(&d, |dx, dy, dz| {
		if (dx, dy, dz) == (0, 1, 0) {
			Src::Chunk(&above)
		} else {
			Src::Air
		}
	});
	let m = mesh_chunk(&input);
	let side = water_quad(&m.water, 8, 15, 8, Dir::West);
	for (_, w1) in side {
		assert_eq!(fall_of(w1), 15, "unknown height above the seam");
	}
}

#[test]
fn adjacent_flows_share_corner_heights() {
	// Flow3 | Flow5 side by side: no interface faces, and the two top
	// faces agree on their shared corners — the strongest amount
	// wins, so the thicker cell holds its own level right up to the
	// seam and the thinner one ramps up to meet it.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::WaterFlow3);
	d.set(9, 8, 8, B::WaterFlow5);
	let m = mesh_chunk(&isolated(&d));
	assert_eq!(m.water.len() / 8, 2 * 5, "interface culled");
	let top3 = water_quad(&m.water, 8, 8, 8, Dir::Top);
	let top5 = water_quad(&m.water, 9, 8, 8, Dir::Top);
	let d_at = |quad: &[(u32, u32); 4], lx: u32| -> Vec<u32> {
		quad.iter()
			.filter(|(w0, _)| x_of(*w0) == lx)
			.map(|(_, w1)| drop_of(*w1))
			.collect()
	};
	assert_eq!(d_at(&top3, 9), vec![4, 4], "shared corners from both");
	assert_eq!(d_at(&top5, 9), vec![4, 4], "identical on the other face");
	assert_eq!(d_at(&top3, 8), vec![6, 6], "flow3 alone: d = 9 − 3");
	assert_eq!(d_at(&top5, 10), vec![4, 4], "flow5 alone: d = 9 − 5");
}

#[test]
fn corner_heights_knit_across_chunk_borders() {
	// Source at the +x border; the neighbor chunk holds Flow4 across
	// the seam. The shell water IS seen — the flow's own outer face
	// drops away to its own level — but the border corners it shares with
	// the source pin full, and the seam agrees from both sides.
	let mut d = ChunkData::default();
	d.set(15, 8, 8, B::Water);
	let mut other = ChunkData::default();
	other.set(0, 8, 8, B::WaterFlow4);
	let input = MeshInput::build(&d, |dx, dy, dz| {
		if (dx, dy, dz) == (1, 0, 0) {
			Src::Chunk(&other)
		} else {
			Src::Air
		}
	});
	let m = mesh_chunk(&input);
	let top = water_quad(&m.water, 15, 8, 8, Dir::Top);
	for (_, w1) in top {
		assert_eq!(drop_of(w1), 1, "a source's top face stays full");
	}
	// The mirror job: the neighbor chunk meshed with our source in
	// ITS shell must see the same seam corners, or the two surfaces
	// crack apart.
	let mirror = MeshInput::build(&other, |dx, dy, dz| {
		if (dx, dy, dz) == (-1, 0, 0) {
			Src::Chunk(&d)
		} else {
			Src::Air
		}
	});
	let flow_top = water_quad(&mesh_chunk(&mirror).water, 0, 8, 8, Dir::Top);
	for (w0, w1) in flow_top {
		let expect = if x_of(w0) == 0 { 1 } else { 5 };
		assert_eq!(drop_of(w1), expect, "seam pins, outer edge slopes away");
	}
}

#[test]
fn a_source_beside_flowing_water_stays_full() {
	// The reported bug: the fluid sim halves on spread, so a source
	// sits next to its own thinning flows. Averaging sank its whole
	// top face deep into the block — a full block has to keep looking
	// full, and the flow still slopes away from it.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Water);
	for (x, z) in [(7, 8), (9, 8), (8, 7), (8, 9)] {
		d.set(x, 8, z, B::WaterFlow4);
	}
	for (x, z) in [(7, 7), (9, 7), (7, 9), (9, 9)] {
		d.set(x, 8, z, B::WaterFlow2);
	}
	let m = mesh_chunk(&isolated(&d));
	for (_, w1) in water_quad(&m.water, 8, 8, 8, Dir::Top) {
		assert_eq!(drop_of(w1), 1, "the source keeps its 8/9 surface");
	}
	// The Flow4 at +x: its uphill corners take the source's height,
	// its downhill ones its OWN — one clean step of the sim's ladder.
	// Averaging with the Flow2 beyond used to carry them below that,
	// so the surface undershot the level it was meant to be showing.
	for (w0, w1) in water_quad(&m.water, 9, 8, 8, Dir::Top) {
		let expect = if x_of(w0) == 9 { 1 } else { 5 };
		assert_eq!(drop_of(w1), expect, "the flow ramps down and away");
	}
	// ...and the Flow2 on the diagonal picks up exactly where the
	// Flow4 left off: one rung of the sim's ladder per corner, with
	// nothing undershooting between them.
	let mut corners = water_quad(&m.water, 9, 8, 7, Dir::Top).map(|(_, w1)| drop_of(w1));
	corners.sort_unstable();
	assert_eq!(corners, [1, 5, 5, 7], "4/9 down to 2/9, no undershoot");
}

fn corner_of(w0: u32) -> u32 {
	(w0 >> 27) & 0x3
}

/// The emitted quad (4 consecutive two-word vertices) for the given
/// block's face toward `dir`, matched by normal + corner lattice
/// positions. Returns the geometry words (w0) — solid tests never
/// inspect w1 beyond the layer, which has its own path.
fn quad_of(m: &[u32], bx: i32, by: i32, bz: i32, dir: Dir) -> [u32; 4] {
	let want: std::collections::HashSet<(u32, u32, u32)> = face_corners(bx, by, bz, dir)
		.iter()
		.map(|&(x, y, z)| (x as u32, y as u32, z as u32))
		.collect();
	for q in m.chunks(8) {
		let got: std::collections::HashSet<(u32, u32, u32)> = q
			.chunks(2)
			.map(|p| (x_of(p[0]), y_of(p[0]), z_of(p[0])))
			.collect();
		if dir_of(q[0]) == dir as u32 && got == want {
			return [q[0], q[2], q[4], q[6]];
		}
	}
	panic!("face of ({bx},{by},{bz}) toward {dir:?} not found");
}

/// Every quad emits its corners in `face_corners` order, TL TR BL BR, with no exceptions. With
/// baked AO gone there is no per-corner signal left to steer the winding, so it is simply fixed —
/// and still has to be pinned: `quad_indices` assumes this order, and a quad wound the other way
/// is deleted by `cull: Back`, invisible in a unit test that only counts faces and obvious as
/// holes on screen.
#[test]
fn every_quad_winds_in_face_corner_order() {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Dirt);
	// A neighbour on one side only, so the old rule would have had an
	// asymmetric AO score to flip on.
	d.set(9, 9, 8, B::Dirt);
	let m = mesh_chunk(&isolated(&d));
	for dir in [
		Dir::West,
		Dir::East,
		Dir::North,
		Dir::South,
		Dir::Top,
		Dir::Bottom,
	] {
		let q = quad_of(&m.solid, 8, 8, 8, dir);
		assert_eq!(
			q.map(corner_of),
			[0, 1, 2, 3],
			"{dir:?} must emit TL TR BL BR"
		);
	}
}

#[test]
fn leaves_mesh_as_solid_blocks() {
	// Two adjacent leaves over dirt: the shared leaf|leaf pair culls,
	// and the leaf|dirt pair culls BOTH ways — a leaf texture has no
	// transparent texel, so nothing may show through one and no face
	// under one is worth emitting.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Leaves);
	d.set(9, 8, 8, B::Leaves);
	d.set(8, 7, 8, B::Dirt);
	let m = mesh_chunk(&isolated(&d));
	assert!(m.water.is_empty(), "leaves mesh into the solid bucket");
	let layer_of = |p: &[u32]| p[1] & 0x3ff;
	let leaf = u32::from(B::Leaves.props().tex_side);
	let dirt = u32::from(B::Dirt.props().tex_side);
	let leaf_faces = m.solid.chunks(2).filter(|p| layer_of(p) == leaf).count() / 4;
	let dirt_faces = m.solid.chunks(2).filter(|p| layer_of(p) == dirt).count() / 4;
	// Leaf A loses the shared face and its bottom; leaf B only the
	// shared one.
	assert_eq!(leaf_faces, 9);
	// The dirt's top face culls under the solid leaf.
	assert_eq!(dirt_faces, 5);
}

/// Every see-through row streams BLENDED, and hides nothing: its faces
/// land in `mesh.translucent` (never solid or water), and a stone beside
/// it keeps the interface face you see through it. Swept over the bucket
/// rather than named blocks, with the sweep counted — a future pane joins
/// the pin by having the bucket at all.
#[test]
fn translucent_rows_stream_blended_and_hide_nothing() {
	use crate::block::MeshBucket;
	let mut examined = 0u32;
	for &b in &B::ALL {
		if b.props().bucket != MeshBucket::Translucent {
			continue;
		}
		examined += 1;
		let mut d = ChunkData::default();
		d.set(8, 8, 8, b);
		d.set(7, 8, 8, B::Stone);
		let m = mesh_chunk(&isolated(&d));
		assert!(!m.translucent.is_empty(), "{b:?} streams translucent");
		assert!(m.water.is_empty(), "{b:?} is not water");
		// The stone's face toward the see-through cell survives — the
		// whole reason a translucent neighbor covers nothing.
		quad_of(&m.solid, 7, 8, 8, Dir::West);
		// And the stone's own faces are the only solid ones: none of
		// this block's leaked into the opaque stream.
		let stone = u32::from(B::Stone.props().tex_side);
		for pair in m.solid.chunks(2) {
			assert_eq!(pair[1] & 0x3ff, stone, "{b:?} leaked a solid face");
		}
	}
	assert!(examined > 0, "the bucket has members to sweep");
}

/// Glass culling is same-type only, and the water interface emits BOTH
/// ways — an aquarium wall shows its water, and the water shows its
/// glass, each blending in its own pass order.
#[test]
fn glass_culls_same_type_and_meets_water_face_to_face() {
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Glass);
	d.set(9, 8, 8, B::Glass);
	d.set(7, 8, 8, B::Water);
	let m = mesh_chunk(&isolated(&d));
	// Two cubes lose exactly the shared pair; the glass|water interface
	// stays — the water sits at 7, so the pane's EAST face and the
	// water's WEST face share that plane and both draw.
	let faces = m.translucent.len() / 8;
	assert_eq!(faces, 10, "two glass cubes cull only their shared pair");
	quad_of(&m.translucent, 8, 8, 8, Dir::East);
	// Water emits toward glass (the extended Translucent arm).
	water_quad(&m.water, 7, 8, 8, Dir::West);
}

/// The see-through predicate is the BUCKET, swept over every block and every valid state. A
/// translucent cell covers NOTHING, so no face rule anywhere can cull the face you look through
/// it at — and the sweep is what generalizes it: three separate sites keyed on the render CLASS
/// instead, and each grew its own hole (a water void behind a crystal slab, a brimming flow
/// under a pane, doubled interior quads).
#[test]
fn a_translucent_cell_never_covers_whatever_its_shape() {
	use crate::block::MeshBucket;
	use crate::dir::DIRS;
	let mut swept = 0u32;
	for b in B::ALL {
		if b.props().bucket != MeshBucket::Translucent {
			continue;
		}
		for state in 0..16u8 {
			let cell = b as u16 | (u32::from(state) << crate::chunk::ID_BITS) as u16;
			if !crate::shapes::state_valid(cell) {
				continue;
			}
			for dir in DIRS {
				assert!(
					matches!(neighbor_cover(cell, dir), Cover::None),
					"{b:?} state {state} covers toward {dir:?}"
				);
				swept += 1;
			}
		}
	}
	// Every translucent row, every valid state, every direction — and the
	// count says the sweep examined them rather than skipping the lot.
	assert!(swept > 0, "the bucket has members to sweep");
}

/// A run of identical translucent SHAPED cells culls its interior faces
/// exactly as a run of translucent cubes does. The two are one rule, so
/// the counts are compared against each other rather than written down:
/// a shaped run that stopped culling would leave coincident quads the
/// no-depth-write glass pass tints twice.
#[test]
fn a_translucent_shaped_run_culls_like_a_translucent_cube_run() {
	let run = |b: B| {
		let mut d = ChunkData::default();
		for x in 6..10 {
			d.set(x, 8, 8, b);
		}
		mesh_chunk(&isolated(&d)).translucent.len() / 8
	};
	let shaped = run(B::CrystalSlab);
	let cubes = run(B::Crystal);
	// Both runs are four cells long and closed, so both present exactly
	// the two end caps plus their four sides — the interior boundaries
	// are gone on both sides of the comparison.
	assert_eq!(
		shaped, cubes,
		"a shaped translucent run must cull its interior faces like a cube run"
	);
	// And the comparison is not vacuous: the run really did mesh.
	assert!(cubes > 0, "test needs a real translucent mesh");
	// A DIFFERENT translucent type still meets face to face — culling is
	// same-type, never "any pane hides any pane".
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::CrystalSlab);
	d.set(9, 8, 8, B::RedGlass);
	let mixed = mesh_chunk(&isolated(&d)).translucent.len() / 8;
	let lone = {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::CrystalSlab);
		mesh_chunk(&isolated(&d)).translucent.len() / 8
	} + {
		let mut d = ChunkData::default();
		d.set(9, 8, 8, B::RedGlass);
		mesh_chunk(&isolated(&d)).translucent.len() / 8
	};
	assert_eq!(mixed, lone, "unlike types cull nothing against each other");
}

/// Water meets a see-through neighbour face to face whatever its SHAPE:
/// the predicate is the bucket, so a crystal slab gets the same water
/// face a glass cube does. Keying it on the class left a water void
/// behind every translucent shaped cover.
#[test]
fn water_meets_every_see_through_neighbor_whatever_its_shape() {
	let faces_toward = |b: B| {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(9, 8, 8, b);
		mesh_chunk(&isolated(&d)).water.len() / 8
	};
	// The bucket decides, so a shaped translucent and a cube translucent
	// must give the water the same face count...
	assert_eq!(
		faces_toward(B::CrystalSlab),
		faces_toward(B::Glass),
		"water treats every translucent neighbour alike"
	);
	// ...and that count is one more than an OPAQUE shaped neighbour,
	// which still covers (the standing v1 artifact).
	assert_eq!(
		faces_toward(B::CrystalSlab),
		faces_toward(B::StoneSlab) + 1,
		"a see-through cover earns the water an extra face"
	);
}

/// Glass over water leaves the surface knitted at its real depth. The
/// full-cube rule keys on whether the cover HIDES the top face, and a
/// pane does not — pinning it full would brim a shallow flow up to a
/// full block under every window.
#[test]
fn a_see_through_cover_leaves_the_water_surface_alone() {
	let drops_under = |above: Option<B>| {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::WaterFlow1);
		if let Some(b) = above {
			d.set(8, 9, 8, b);
		}
		let m = mesh_chunk(&isolated(&d));
		// The four corner codes of the top face carry the surface drop.
		m.water
			.chunks(2)
			.filter(|p| dir_of(p[0]) as usize == Dir::Top as usize)
			.map(|p| drop_of(p[1]))
			.collect::<Vec<_>>()
	};
	let open = drops_under(None);
	assert!(!open.is_empty(), "test needs a real water surface");
	assert_eq!(
		drops_under(Some(B::Glass)),
		open,
		"a pane over water must not pin the surface full"
	);
	// An OPAQUE cover still pins it — the rule moved, it did not vanish.
	assert_ne!(
		drops_under(Some(B::Stone)),
		open,
		"an opaque cover still renders the cell full"
	);
}

#[test]
fn grass_bottom_uses_dirt_texture() {
	// Per-face table: grass top/side/bottom are three distinct layers.
	let mut d = ChunkData::default();
	d.set(8, 8, 8, B::Grass);
	let m = mesh_chunk(&isolated(&d));
	// The layers are read off the block's own row, which IS the per-face
	// table this is about — so the test proves the mesher consults it
	// rather than re-typing its contents.
	let props = B::Grass.props();
	for p in m.solid.chunks(2) {
		let expect = match dir_of(p[0]) as usize {
			n if n == Dir::Top as usize => props.tex_top,
			n if n == Dir::Bottom as usize => props.tex_bottom,
			_ => props.tex_side,
		};
		assert_eq!(p[1] & 0x3ff, u32::from(expect));
	}
	// And the three really are distinct, or the loop above proves little.
	assert_ne!(props.tex_top, props.tex_side);
	assert_ne!(props.tex_bottom, props.tex_side);
}
