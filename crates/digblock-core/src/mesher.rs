//! Chunk meshing from immutable snapshots. Replaces the Java pattern of
//! writing face bits into neighbor chunks: a mesh job gets a padded 18³
//! copy of its chunk plus a one-block shell from all 26 neighbors — no
//! shared mutation, no locks, and diagonal neighbors are available for
//! per-vertex AO and water corner heights.
//!
//! Face rules (originally distilled from Chunk.setFlags in the Java, now
//! driven by each block's RenderClass — see block.rs), verified by tests:
//! - an Opaque neighbor occludes any face
//! - Opaque emits toward any non-occluding neighbor; Cutout the same except
//!   against its own type; Translucent emits only toward class-None (air)
//!   and never against its own type — so water|water yields nothing
//! - a missing (unloaded) neighbor suppresses border faces — whichever
//!   chunk generates second re-dirties this one, matching the Java
//! - above the top of the world there is an air sentinel; below the bottom,
//!   missing (the Java null) — so no bottom faces at world bottom

use crate::block::{BlockType, RenderClass};
use crate::chunk::{ChunkData, CHUNK_SIZE};

mod water;
use water::{emit_water_face, FallLayers, WaterState};

use crate::dir::{Dir, DIRS};

/// Padded snapshot edge: the chunk plus a one-block shell from its
/// neighbors.
pub const PADDED: usize = CHUNK_SIZE + 2;
const PADDED_VOLUME: usize = PADDED * PADDED * PADDED;

/// Where a neighbor chunk's blocks come from when building a `MeshInput`,
/// keyed by chunk offset (dx, dy, dz) ∈ {-1,0,1}³.
pub enum Src<'a> {
	/// A generated, non-sentinel neighbor chunk.
	Chunk(&'a ChunkData),
	/// All air: the sky sentinel, or above the world top.
	Air,
	/// Not generated (or below the world bottom): shell cells read as air;
	/// on a face offset it also suppresses that border's faces (Java null
	/// neighbor — whichever chunk generates second re-dirties this one).
	Missing,
}

/// A mesh job's complete input: 18³ block ids (the chunk plus a one-block
/// shell copied from every existing neighbor — faces, edges, corners) and
/// a missing flag per face.
pub struct MeshInput {
	/// Indexed by `pidx` over coords in -1..=16; shell cells with no
	/// backing chunk are air (0).
	pub blocks: Box<[u16; PADDED_VOLUME]>,
	/// Face neighbor not loaded: suppress that border's faces.
	pub missing: [bool; 6],
}

/// Padded flat index for coords in -1..=16 — same y,z,x nesting as
/// `ChunkData::index`, x contiguous.
#[inline]
pub fn pidx(x: i32, y: i32, z: i32) -> usize {
	debug_assert!((-1..=16).contains(&x) && (-1..=16).contains(&y) && (-1..=16).contains(&z));
	let n = PADDED as i32;
	(((y + 1) * n + (z + 1)) * n + (x + 1)) as usize
}

/// The face `Dir` for an axis-aligned unit chunk offset; None for
/// edge/corner offsets.
fn face_dir(dx: i32, dy: i32, dz: i32) -> Option<Dir> {
	match (dx, dy, dz) {
		(1, 0, 0) => Some(Dir::West),
		(-1, 0, 0) => Some(Dir::East),
		(0, 0, 1) => Some(Dir::North),
		(0, 0, -1) => Some(Dir::South),
		(0, 1, 0) => Some(Dir::Top),
		(0, -1, 0) => Some(Dir::Bottom),
		_ => None,
	}
}

impl MeshInput {
	/// Assemble from the center chunk plus a lookup over the 26 neighbor
	/// chunk offsets. Air and Missing regions stay zero-filled (air) —
	/// only the face missing flags distinguish them.
	pub fn build<'a>(
		center: &ChunkData,
		mut neighbor: impl FnMut(i32, i32, i32) -> Src<'a>,
	) -> MeshInput {
		let n = CHUNK_SIZE;
		let mut input = MeshInput {
			blocks: Box::new([0u16; PADDED_VOLUME]),
			missing: [false; 6],
		};
		for y in 0..n {
			for z in 0..n {
				let src = (y * n + z) * n;
				let dst = pidx(0, y as i32, z as i32);
				input.blocks[dst..dst + n].copy_from_slice(&center.blocks[src..src + n]);
			}
		}
		// The local index range a neighbor at axis offset d contributes:
		// its far border row for -1, its near border row for +1.
		let span = |d: i32| match d {
			-1 => 15..16,
			1 => 0..1,
			_ => 0..16,
		};
		for dy in -1..=1 {
			for dz in -1..=1 {
				for dx in -1..=1 {
					if (dx, dy, dz) == (0, 0, 0) {
						continue;
					}
					let d = match neighbor(dx, dy, dz) {
						Src::Chunk(d) => d,
						Src::Air => continue,
						Src::Missing => {
							if let Some(dir) = face_dir(dx, dy, dz) {
								input.missing[dir as usize] = true;
							}
							continue;
						}
					};
					for ly in span(dy) {
						for lz in span(dz) {
							for lx in span(dx) {
								input.blocks[pidx(
									lx as i32 + 16 * dx,
									ly as i32 + 16 * dy,
									lz as i32 + 16 * dz,
								)] = d.blocks[(ly * n + lz) * n + lx];
							}
						}
					}
				}
			}
		}
		input
	}

	/// Block id at padded coords (each in -1..=16) — the state nibble is
	/// masked HERE, so every id-oriented consumer (AO, the whole water
	/// surface system) is state-blind by construction. The shaped path
	/// reads raw cells via [`Self::get_cell`].
	#[inline]
	pub fn get(&self, x: i32, y: i32, z: i32) -> u16 {
		self.blocks[pidx(x, y, z)] & crate::chunk::ID_MASK
	}

	/// Raw cell (id + state) at padded coords.
	#[inline]
	pub fn get_cell(&self, x: i32, y: i32, z: i32) -> u16 {
		self.blocks[pidx(x, y, z)]
	}
}

pub struct MeshData {
	/// Packed two-word vertices (see `pack`), 4 per face (TL,TR,BL,BR).
	pub solid: Vec<u32>,
	/// Same two-word format; word 1 additionally carries the water
	/// payload above the layer field, shifted by 10: bits 10-13 surface
	/// drop code d (the shader lowers the vertex by d/9 — 0 = full
	/// height), bit 14 falling, bits 15-17 flow dir, bits 18-19 the
	/// splash code (3 = fall impact cell, 2/1 = pool cells at Chebyshev
	/// distance 1/2 — expanding foam rings), bits 20-23 the vertex fall
	/// distance (blocks of falling water above; bottom-plane verts +1 so
	/// the interpolated "blocks fallen" is continuous down a column;
	/// saturates at 15).
	pub water: Vec<u32>,
	/// Water-fall impact cells (the splash-code-3 predicate), packed
	/// chunk-local `x | y<<4 | z<<8` — spray-emitter metadata. Rides the
	/// mesh so every fall-changing path (worldgen stream-in, dig/place,
	/// fluid sim, remesh-all) refreshes emitters with no extra scans.
	pub impacts: Vec<u16>,
}

impl MeshData {
	pub fn is_empty(&self) -> bool {
		self.solid.is_empty() && self.water.is_empty()
	}
}

/// What a neighbor cell presents against a face looking toward `dir` —
/// i.e. its coverage of the shared boundary plane (its face toward
/// `dir.opposite()`).
enum Cover {
	None,
	Full,
	Partial(&'static crate::shapes::FaceMask),
}

fn neighbor_cover(cell: u16, dir: Dir) -> Cover {
	let b = BlockType::from_id(crate::chunk::cell_id(cell));
	match b.props().class {
		RenderClass::Opaque => Cover::Full,
		RenderClass::Shaped => {
			let v = crate::shapes::shape_of(b, crate::chunk::cell_state(cell))
				.expect("Shaped class ⇒ registered shape");
			let opp = dir.opposite() as usize;
			if v.occludes_full[opp] {
				Cover::Full
			} else {
				Cover::Partial(&v.face_masks[opp])
			}
		}
		_ => Cover::None,
	}
}

#[inline]
fn face_visible(me: BlockType, neighbor: Option<u16>, dir: Dir) -> bool {
	let Some(nc) = neighbor else { return false }; // Missing neighbor
	if matches!(neighbor_cover(nc, dir), Cover::Full) {
		return false;
	}
	let nb = BlockType::from_id(crate::chunk::cell_id(nc));
	match me.props().class {
		RenderClass::None => false,
		RenderClass::Opaque => true,
		RenderClass::Cutout => nb != me,
		// Water toward a Shaped neighbor emits nothing (class != None) —
		// the known v1 artifact, pinned by test: you can see through a
		// stair's open region into the water void behind it.
		RenderClass::Translucent => nb.props().class == RenderClass::None,
		// Shaped cells never route here — mesh_chunk branches to the
		// shaped emitter first.
		RenderClass::Shaped => unreachable!("shaped cells use emit_shaped"),
	}
}

/// Pack a vertex's GEOMETRY word (word 0 — solid and water share it).
/// Bits 0-7 x, 8-15 y, 16-23 z in TWELFTH-block units 0..=192 (the
/// sub-block shape grid; full-block geometry lands on multiples of 12),
/// 24-26 normal, 27-28 corner (0=TL 1=TR 2=BL 3=BR), 29-31 free.
///
/// Word 1 is per-stream material, assembled by the caller:
/// solid — [`solid_word1`]: bits 0-9 texture layer (1024 cap =
/// MAX_TEX_LAYERS), 10-13 uv u in twelfths (0..=12), 14-17 uv v,
/// 18 emissive (Stage-A fullbright — chunk.wgsl skips the lighting
/// terms), 19-31 free. water — bits 0-9 texture layer, 10-23 payload
/// (see MeshData::water), 24-31 free (water reads its uv from the
/// corner code).
///
/// This layout is what the demolition left: the AO pair, the three
/// Stage-B light channels and the per-block tint jitter all came out
/// together, so the format was repacked tight rather than left holed.
/// Every consumer moves with it — chunk.wgsl's `vs` and `vs_depth`,
/// water.wgsl, and the renderer's vertex attributes.
#[inline]
fn pack(pos: (u32, u32, u32), normal: Dir, corner: u32) -> u32 {
	let (x, y, z) = pos;
	debug_assert!(x <= 192 && y <= 192 && z <= 192 && corner < 4);
	x | (y << 8) | (z << 16) | ((normal as u32) << 24) | (corner << 27)
}

/// The solid stream's material word (see [`pack`] for the layout).
#[inline]
fn solid_word1(tex: u32, uv_u: u32, uv_v: u32, emissive: bool) -> u32 {
	debug_assert!((tex as usize) < crate::block::MAX_TEX_LAYERS);
	debug_assert!(uv_u <= 12 && uv_v <= 12);
	tex | (uv_u << 10) | (uv_v << 14) | ((emissive as u32) << 18)
}

/// One vertex's geometry attributes on their way into `pack`. Positions
/// are in twelfths of a block (0..=192 across the chunk).
/// Corner lattice positions [TL, TR, BL, BR] of the face of block
/// (x, y, z) toward `dir` — the Java addFaces geometry (per-face origin
/// offset + diagonal vector).
///
/// Public because this order and [`quad_indices`] are one contract: any
/// quad drawn through that index buffer must come out of this table, or
/// it winds inward and `cull: Back` deletes it. The render crate's item
/// geometry builds its boxes against this.
pub fn face_corners(x: i32, y: i32, z: i32, dir: Dir) -> [(i32, i32, i32); 4] {
	let ((ox, oy, oz), (dx, dy, dz)) = match dir {
		Dir::South => ((0, 0, 0), (1, 1, 0)),
		Dir::East => ((0, 0, 1), (0, 1, -1)),
		Dir::North => ((1, 0, 1), (-1, 1, 0)),
		Dir::West => ((1, 0, 0), (0, 1, 1)),
		Dir::Top => ((0, 1, 0), (1, 0, 1)),
		Dir::Bottom => ((0, 0, 1), (1, 0, -1)),
	};
	let (x, y, z) = (x + ox, y + oy, z + oz);
	let vertical = matches!(dir, Dir::Top | Dir::Bottom);
	[
		(x + dx, y + dy, z + dz),
		(x, y + dy, if vertical { z + dz } else { z }),
		(x + dx, y, if vertical { z } else { z + dz }),
		(x, y, z),
	]
}

/// Whether a block id casts ambient occlusion. Opaque only — cutout and
/// translucent neighbors don't occlude faces (`face_visible`), so they
/// shouldn't darken corners you can see through them.
#[inline]
fn occludes(id: u16) -> bool {
	BlockType::from_id(id).props().class == RenderClass::Opaque
}

/// Emit one face quad in `face_corners` order (TL TR BL BR), which
/// [`quad_indices`] triangulates as the diagonal between emit slots 0
/// and 3.
///
/// The order used to FLIP to [1,3,0,2] so that diagonal crossed the
/// lower-AO-sum corner pair, turning a single dark corner into a radial
/// gradient instead of a hard crease. With baked AO gone there is no
/// per-corner signal left to steer it, so the winding is simply fixed —
/// pinned by `every_quad_winds_in_face_corner_order`, because a quad
/// wound the other way is deleted by `cull: Back`.
fn emit_face(out: &mut Vec<u32>, b: (usize, usize, usize), dir: Dir, tex: u32, emissive: bool) {
	let corners = face_corners(b.0 as i32, b.1 as i32, b.2 as i32, dir);
	for (ci, &(cx, cy, cz)) in corners.iter().enumerate() {
		let ci = ci as u32;
		out.push(pack(
			(cx as u32 * 12, cy as u32 * 12, cz as u32 * 12),
			dir,
			ci,
		));
		// Full-rect uv straight from the corner code, exactly the mapping
		// the shader used to derive itself.
		out.push(solid_word1(tex, (ci & 1) * 12, (ci >> 1) * 12, emissive));
	}
}

/// Stamp a shaped block's baked quad template. Outlined like the water
/// path — shaped cells are rare and the solid loop is hot. Interior quads
/// always emit; boundary quads cull against the neighbor's coverage of
/// the shared plane (mask-driven, so stacked mirrored stairs and
/// stair-against-wall cull correctly, cross-chunk included — the padded
/// shell carries state).
#[inline(never)]
fn emit_shaped(
	out: &mut Vec<u32>,
	input: &MeshInput,
	b: (usize, usize, usize),
	me: BlockType,
	cell: u16,
) {
	let variant = crate::shapes::shape_of(me, crate::chunk::cell_state(cell))
		.expect("Shaped class ⇒ registered shape");
	let (bx, by, bz) = (b.0 as i32, b.1 as i32, b.2 as i32);
	let n = CHUNK_SIZE;
	for q in &variant.quads {
		if q.boundary {
			let dir = q.dir;
			let on_border = match dir {
				Dir::West => b.0 == n - 1,
				Dir::North => b.2 == n - 1,
				Dir::South => b.2 == 0,
				Dir::Bottom => b.1 == 0,
				Dir::Top => b.1 == n - 1,
				Dir::East => b.0 == 0,
			};
			if on_border && input.missing[dir as usize] {
				continue;
			}
			let (ox, oy, oz) = dir.offset();
			match neighbor_cover(input.get_cell(bx + ox, by + oy, bz + oz), dir) {
				Cover::Full => continue,
				Cover::Partial(m) if m.covers(q.rect[0], q.rect[1], q.rect[2], q.rect[3]) => {
					continue
				}
				_ => {}
			}
		}
		let tex = me.texture(q.dir);
		for ci in 0..4usize {
			let corner = q.corners[ci];
			out.push(pack(
				(
					bx as u32 * 12 + u32::from(corner[0]),
					by as u32 * 12 + u32::from(corner[1]),
					bz as u32 * 12 + u32::from(corner[2]),
				),
				q.dir,
				ci as u32,
			));
			out.push(solid_word1(
				tex,
				u32::from(q.uv[ci][0]),
				u32::from(q.uv[ci][1]),
				me.props().emissive,
			));
		}
	}
}

/// Flat-index delta to the padded neighbor cell, per `Dir`.
const PDELTA: [isize; 6] = [
	1,                             // West  (+x)
	PADDED as isize,               // North (+z)
	-(PADDED as isize),            // South (−z)
	-((PADDED * PADDED) as isize), // Bottom (−y)
	(PADDED * PADDED) as isize,    // Top   (+y)
	-1,                            // East  (−x)
];

/// Mesh a chunk from its padded snapshot. Output is chunk-local: the
/// `base` world origin this used to take left with the vertex-packed tint
/// hash, and the world position a shader needs now comes from the
/// instance-step origin buffer instead.
pub fn mesh_chunk(input: &MeshInput) -> MeshData {
	let n = CHUNK_SIZE;
	// Sized for a busy surface chunk; realloc past this is rare.
	let mut mesh = MeshData {
		solid: Vec::with_capacity(4096),
		water: Vec::with_capacity(512),
		impacts: Vec::new(),
	};
	let mut falls = FallLayers::default();
	for y in 0..n {
		for z in 0..n {
			for x in 0..n {
				let pi = pidx(x as i32, y as i32, z as i32);
				let cell = input.blocks[pi];
				let me = BlockType::from_id(crate::chunk::cell_id(cell));
				if me.props().class == RenderClass::None {
					continue;
				}
				if me.props().class == RenderClass::Shaped {
					emit_shaped(&mut mesh.solid, input, (x, y, z), me, cell);
					continue;
				}
				// A fall's landing cell drives the spray emitters and the
				// waterfall audio, so it is recorded per CELL — never as a
				// side effect of emitting a face, which is how it used to
				// work and why a fall landing inside its own pool was
				// silent and dry (see `water::is_impact`).
				if me.is_falling() && water::is_impact(input, x as i32, y as i32, z as i32) {
					mesh.impacts.push((x | y << 4 | z << 8) as u16);
				}
				// Cell-level water state, shared by all its faces.
				let mut wstate: Option<WaterState> = None;
				// Per-cell brightness jitter, hashed at most once for all
				// six faces (the shaped path hoists it the same way).
				// Lazy, not eager: a buried cell emits nothing and must
				// not pay for a hash nobody reads.
				for &dir in &DIRS {
					// A missing face neighbor suppresses this border's
					// faces; the padded ring makes every other lookup a
					// uniform flat index.
					let on_border = match dir {
						Dir::West => x == n - 1,
						Dir::North => z == n - 1,
						Dir::South => z == 0,
						Dir::Bottom => y == 0,
						Dir::Top => y == n - 1,
						Dir::East => x == 0,
					};
					let neighbor = if on_border && input.missing[dir as usize] {
						None
					} else {
						let ni = (pi as isize + PDELTA[dir as usize]) as usize;
						Some(input.blocks[ni])
					};
					if !face_visible(me, neighbor, dir) {
						continue;
					}
					let tex = me.texture(dir);
					if me.props().bucket == crate::block::MeshBucket::Water {
						let state = *wstate.get_or_insert_with(|| {
							WaterState::of(input, &mut falls, x as i32, y as i32, z as i32, me)
						});
						emit_water_face(&mut mesh.water, input, (x, y, z), dir, tex, state);
					} else {
						emit_face(&mut mesh.solid, (x, y, z), dir, tex, me.props().emissive);
					}
				}
			}
		}
	}
	mesh
}

/// The static index pattern shared by every chunk mesh: for quad q with
/// vertex base q*4, indices [base+3, base+1, base+0, base+3, base+0,
/// base+2] — the Java triangle order (BR,TR,TL)+(BR,TL,BL), CCW.
pub fn quad_indices(max_quads: usize) -> Vec<u32> {
	let mut idx = Vec::with_capacity(max_quads * 6);
	for q in 0..max_quads as u32 {
		let b = q * 4;
		idx.extend_from_slice(&[b + 3, b + 1, b, b + 3, b, b + 2]);
	}
	idx
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType as B;

	fn solo(block: B) -> ChunkData {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, block);
		d
	}

	/// All 26 neighbors missing — the old ALL_MISSING fixture.
	fn isolated(d: &ChunkData) -> MeshInput {
		MeshInput::build(d, |_, _, _| Src::Missing)
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

	/// A stair at each state, meshed solo (chunk-local (8,8,8), world
	/// origin ZERO).
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
		use crate::shapes::{stair_state, Facing};
		// Stair ascending +x with an opaque block at +x: the stair's full
		// back face is culled, and the wall's −x face is culled too (the
		// stair's back fully covers the shared plane).
		let mut d = ChunkData::default();
		d.set_cell(8, 8, 8, make_cell(B::WoodStairs, stair_state(Facing::PosX)));
		d.set(9, 8, 8, B::Stone);
		let m = mesh_chunk(&isolated(&d));
		let stair_back = m.solid.chunks(8).any(|q| {
			dir_of(q[0]) == Dir::West as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 9 * 12)
		});
		assert!(!stair_back, "stair back culled against the wall");
		let wall_west_face = m.solid.chunks(8).any(|q| {
			dir_of(q[0]) == Dir::East as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 9 * 12)
		});
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
		use crate::shapes::{stair_state, Facing};
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
				if dx == 1 {
					Src::Chunk(&wall)
				} else {
					Src::Air
				}
			},
		);
		let m = mesh_chunk(&input);
		let back = m.solid.chunks(8).any(|q| {
			dir_of(q[0]) == Dir::West as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 16 * 12)
		});
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
		let interface = m.water.chunks(8).any(|q| {
			dir_of(q[0]) == Dir::West as u32 && q.chunks(2).all(|p| (p[0] & 0xff) == 8 * 12)
		});
		assert!(!interface, "no water face against the shaped cell");
	}

	#[test]
	fn water_only_faces_air() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Water);
		d.set(8, 7, 8, B::Dirt); // below
		let m = mesh_chunk(&isolated(&d));
		// Water: 5 air-adjacent sides + top = faces toward air on all open
		// sides. Dirt below: 5 visible faces (top hidden? no — top borders
		// water, water is not solid → dirt top IS visible through water).
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
		below.set(5, 15, 7, B::Stone); // → padded (5, -1, 7)
		let mut edge = ChunkData::default();
		edge.set(0, 8, 0, B::Dirt); // offset (1,0,1) → padded (16, 8, 16)
		let mut corner = ChunkData::default();
		corner.set(0, 0, 0, B::Sand); // offset (1,1,1) → padded (16,16,16)
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
		// Layer 700 proves the 10-bit field (the old format capped at 64).
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
		let w1 = solid_word1(700, 12, 0, false);
		assert_eq!(w1 & 0x3ff, 700);
		assert_eq!((w1 >> 10) & 0xf, 12, "uv u in twelfths");
		assert_eq!((w1 >> 14) & 0xf, 0, "uv v in twelfths");
		assert_eq!((w1 >> 18) & 1, 0, "emissive bit clear");
		assert_eq!(w1 >> 19, 0, "bits 19-31 are free after the repack");
		let lit = solid_word1(700, 12, 0, true);
		assert_eq!((lit >> 18) & 1, 1, "emissive bit set");
	}

	// Decode helpers — the mirror of `pack`/`solid_word1`. Positions come
	// back in whole blocks (every full-block lattice point is a multiple
	// of 12 twelfths).
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

	#[test]
	fn water_word_count_is_even() {
		let m = mesh_chunk(&isolated(&solo(B::Water)));
		assert_eq!(m.water.len() % 8, 0, "whole two-word quads only");
		assert!(m.solid.is_empty());
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
		// Lone source with air above: top face all d = 9 − 8 = 1; side
		// faces drop only their top-plane vertices; nothing is falling.
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
		// The flooded-cave regression: water under an opaque ceiling
		// beside an air column with water directly below it (water
		// pouring through a dug hole into a cave). The corner plunge
		// (d = 9) collapsed every side face to zero area while the
		// ceiling occluded the top — the cell rendered NOTHING and you
		// saw straight through it into the water behind.
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
		// its top-plane vertices take the corner surface (amount 8 → d=1)
		// like any surface water; everything is flagged falling.
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
		// The fluid sim's staircase shape: flow on the upper step, flow
		// one down and one across on the lower step. The upper surface's
		// downhill corners plunge the whole block (d = 9, air with water
		// below) and the lower cell's uphill corners pin full (water
		// hangs over the stone step), so the two surfaces meet exactly
		// at the lattice — a connected ramp, no cliff sliver between.
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
		// Top fall block beside the upper pool's surface source — the
		// worldgen lip arrangement. The interface face is culled, so the
		// two top surfaces must agree on their shared corners or the rim
		// is a see-through crack. The fall surfaces one level below its
		// feeder (amount 8 → 7), so the shared corners average to d = 1
		// while the lip's outer edge dips to d = 2 — the surface tips
		// over the edge instead of popping back up to full height.
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

	/// A source poured against a wall face, with nothing solid under it:
	/// the sim sends it straight down and only the landing spreads, so
	/// the placed cell is ringed by air at its own level with the pool
	/// it just poured one below — the plunge trigger, on all four
	/// corners at once. The source must still render as a full block. It
	/// didn't: the plunge outranked the source pin, so the top face went
	/// to zero height and every side face was then skipped as zero-area,
	/// and the block you had just placed rendered nothing at all.
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
		// slope one level below the feeder (amount 5 → lip 4).
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
		// Flow5 walled in by water except +x: flows toward +x (code 1).
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
		// The shape a player-built waterfall actually makes: the fall
		// punches to the floor and the pool it spreads closes around it,
		// so the impact cell is capped by the fall above and ringed by
		// water — EVERY face culled. Recording the impact used to hang
		// off emitting a face, so this produced no spray and no
		// waterfall audio, while worldgen falls (which land in open air
		// one cell above a pool) worked and hid it.
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
		// Fall into the middle of a 7×7 pool: surface cells ring the
		// impact by Chebyshev distance — 1 → code 2, 2 → code 1,
		// further → 0. Mid-fall cells above the impact stay ring-free.
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

	/// A coastal/ocean-shaped chunk: a full 16×16 two-deep pool on a
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

	/// The splash ring scan's fast path (skipping the 5×5×2 neighborhood
	/// walk when no layer in reach holds falling water) must leave every
	/// stream byte-identical — the impacts list drives spray emitters AND
	/// waterfall audio, so a miss is silent in the mesh and audible in
	/// the world.
	///
	/// Re-pinned at the M1 demolition (GI-DESIGN.md): the vertex format
	/// repack moved the corner code from bits 29-30 to 27-28 and dropped
	/// AO, the light nibbles and the tint, so every word changed. Quad
	/// COUNTS are unchanged (4704 / 3224 / 2) — which is the part that
	/// matters here, since the demolition removed shading, not geometry.
	/// A count that moves means a culling or emission change and must not
	/// be re-pinned without finding out why.
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
		// Fall column y 8..=12: each cell counts the falling cells above
		// it; side-face top-plane verts carry that count, bottom-plane
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
		// A full-height column: the deepest cells cap at 15 instead of
		// wrapping the 4-bit field.
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
		// faces agree on their shared corners (strongest amount 5 →
		// d = 4, so the thicker cell holds its own level right up to the
		// seam and the thinner one ramps up to meet it).
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
		// drops to d = 5 below — but the border corners it shares with
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
		// sits next to 4s and 2s. Averaging sank its whole top face to
		// d = 4 (0.63 of a block) — a full block has to keep looking
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
		// The Flow4 at +x: its uphill corners take the source's 8/9, its
		// downhill ones its OWN 4/9 — one clean step of the sim's
		// ladder. Averaging with the Flow2 beyond used to carry them on
		// down to 3/9, so the surface undershot the level it was meant
		// to be showing.
		for (w0, w1) in water_quad(&m.water, 9, 8, 8, Dir::Top) {
			let expect = if x_of(w0) == 9 { 1 } else { 5 };
			assert_eq!(drop_of(w1), expect, "the flow ramps down and away");
		}
		// ...and the Flow2 on the diagonal picks up exactly where the
		// Flow4 left off: the ladder's 8/9, 4/9, 4/9 and 2/9 rungs, one
		// per corner, with nothing undershooting between them.
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

	/// Every quad emits its corners in `face_corners` order, TL TR BL BR,
	/// with no exceptions.
	///
	/// This replaces the old `quad_flips_toward_dark_tr_bl_diagonal`. The
	/// emit order used to flip to [1,3,0,2] so the triangulation diagonal
	/// crossed the darker AO corner pair; with baked AO gone there is no
	/// per-corner signal left to steer it and the winding is simply fixed.
	/// It still has to be pinned: `quad_indices` assumes this order, and a
	/// quad wound the other way is deleted by `cull: Back` — invisible in
	/// a unit test that only counts faces, obvious as holes on screen.
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
	fn cutout_culls_same_type_but_not_others() {
		// Two adjacent leaves over dirt: the shared leaf|leaf face pair is
		// culled, leaves never occlude the dirt below, and dirt (Opaque)
		// occludes nothing of itself.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Leaves);
		d.set(9, 8, 8, B::Leaves);
		d.set(8, 7, 8, B::Dirt);
		let m = mesh_chunk(&isolated(&d));
		assert!(m.water.is_empty(), "cutout meshes into the solid bucket");
		let layer_of = |p: &[u32]| p[1] & 0x3ff;
		let leaf_faces = m.solid.chunks(2).filter(|p| layer_of(p) == 8).count() / 4;
		let dirt_faces = m.solid.chunks(2).filter(|p| layer_of(p) == 1).count() / 4;
		// Leaf A: 4 (leaf neighbor culled, dirt below occludes). Leaf B: 5.
		assert_eq!(leaf_faces, 9);
		// Dirt: 6 — its top face shows through the non-occluding leaf.
		assert_eq!(dirt_faces, 6);
	}

	#[test]
	fn grass_bottom_uses_dirt_texture() {
		// Per-face table: grass top/side/bottom are three distinct layers.
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Grass);
		let m = mesh_chunk(&isolated(&d));
		for p in m.solid.chunks(2) {
			let expect = match dir_of(p[0]) as usize {
				n if n == Dir::Top as usize => 3,    // GrassTop
				n if n == Dir::Bottom as usize => 1, // Dirt
				_ => 2,                              // GrassSide
			};
			assert_eq!(p[1] & 0x3ff, expect);
		}
	}
}
