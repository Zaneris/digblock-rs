//! Chunk meshing from immutable snapshots: a mesh job gets a padded copy of
//! its chunk plus a one-block shell from every neighbour it touches — faces,
//! edges and corners — so there is no shared mutation and no locks, and
//! diagonal neighbours are available for water corner heights.
//!
//! Face rules, driven by each block's RenderClass and verified by tests: an
//! Opaque neighbour occludes any face; Opaque emits toward any non-occluding
//! neighbour, Cutout the same except against its own type, Translucent only
//! toward air and never against its own type, so water|water yields nothing. A
//! missing (unloaded) neighbour suppresses border faces — whichever chunk
//! generates second re-dirties this one. Above the top of the world there is
//! an air sentinel; below the bottom, missing, so no bottom faces at world
//! bottom.

use crate::block::{BlockType, RenderClass};
use crate::chunk::{CHUNK_SIZE, ChunkData};

pub mod lights;
mod water;
use water::{FallLayers, WaterState, emit_water_face};

use crate::dir::{DIRS, Dir};
use crate::worldgen::Seed;

/// Padded snapshot edge: the chunk plus a one-block shell from its
/// neighbors.
pub const PADDED: usize = CHUNK_SIZE + 2;
pub const PADDED_VOLUME: usize = PADDED * PADDED * PADDED;

/// Where a neighbor chunk's blocks come from when building a `MeshInput`,
/// keyed by chunk offset (dx, dy, dz) ∈ {-1,0,1}³.
pub enum Src<'a> {
	/// A generated, non-sentinel neighbor chunk.
	Chunk(&'a ChunkData),
	/// All air: the sky sentinel, or above the world top.
	Air,
	/// Not generated: shell cells read as air, and a face offset also suppresses that border.
	Missing,
}

/// A mesh job's complete input: the padded block ids (the chunk plus a
/// one-block shell copied from every existing neighbor — faces, edges,
/// corners) and a missing flag per face.
pub struct MeshInput {
	/// Indexed by `pidx` over the padded coordinate range; shell cells
	/// with no backing chunk read as air.
	pub blocks: Box<[u16; PADDED_VOLUME]>,
	/// Face neighbor not loaded: suppress that border's faces.
	pub missing: [bool; 6],
	/// The node reveal (SKILLS-DESIGN §5.3, generalized to every node
	/// kind), when this job's owner wants one. Consumed by
	/// [`MeshInput::apply_nodes`] in the WORKER, before any meshing — see
	/// that method for why the stamp writes block ids.
	pub nodes: Option<NodeStamp>,
}

/// Everything the reveal needs, gathered on the main thread (which is the
/// only place that knows what has been edited) and carried to the worker.
pub struct NodeStamp {
	pub seed: Seed,
	/// World position of the padded array's low corner — the shell cell,
	/// not the chunk's own origin.
	pub origin: (i32, i32, i32),
	/// The looker's two reveal levels — Mining for the ore tiers, Alchemy
	/// for the surface nodes. Two players standing in the same cave mesh
	/// the same chunk differently, which is the feature.
	pub levels: crate::worldgen::nodes::Levels,
	/// Which host families this box was found to hold, decided on the main thread while the
	/// snapshot was assembled. Carried rather than re-derived because `Pipeline::node_stamp`
	/// already walked the ids to know whether a stamp was worth making, so asking again in the
	/// worker is a second full scan for an answered question. It is also the ONLY gate that prunes
	/// anything — the reveal levels cannot, since the first surface node reveals at level one.
	pub hosts: NodeHosts,
	/// Padded indices of cells this world has ever written, SORTED — the
	/// anti-dupe's edit gate. Almost always empty, and bounded by the
	/// edits inside one padded box even in a heavily built area.
	pub edited: Vec<u16>,
}

/// Which of the node layer's three HOST families a padded box holds: only
/// a Stone cell can carry a deposit, only Wood a heart, only Sand a
/// spring. Every false is a whole pass [`MeshInput::apply_nodes`] skips.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct NodeHosts {
	pub deposits: bool,
	pub heartwood: bool,
	pub springs: bool,
}

impl NodeHosts {
	/// Nothing to stamp: the caller should make no stamp at all.
	pub fn is_empty(self) -> bool {
		!self.deposits && !self.heartwood && !self.springs
	}
}

/// Padded flat index over the shell-inclusive coordinate range — same
/// y,z,x nesting as `ChunkData::index`, x contiguous.
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
	/// Assemble from the center chunk plus a lookup over every neighbor
	/// chunk offset. Air and Missing regions stay zero-filled (air) —
	/// only the face missing flags distinguish them.
	pub fn build<'a>(
		center: &ChunkData,
		mut neighbor: impl FnMut(i32, i32, i32) -> Src<'a>,
	) -> MeshInput {
		let n = CHUNK_SIZE;
		let mut input = MeshInput {
			blocks: Box::new([0u16; PADDED_VOLUME]),
			missing: [false; 6],
			nodes: None,
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

	/// Block id at padded coords, shell included — the state nibble is
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

	/// The node reveal (SKILLS-DESIGN §5.3, generalized): rewrite qualifying pristine HOST cells
	/// to their node's BLOCK ID, before anything looks at this array — deposits in Stone,
	/// heartwood in trunk Wood, springstone in bed Sand. Stamping ids rather than texture layers
	/// is the whole trick: the mesher, the texture path, the emissive bit, `block_tint` and shaped
	/// culling all simply see a real node block and need no change whatsoever.
	pub fn apply_nodes(&mut self) {
		use crate::worldgen::{nodes, ore};
		let Some(stamp) = self.nodes.take() else {
			return;
		};
		let (ox, oy, oz) = stamp.origin;
		let span = PADDED as i32 - 1;
		let stone = BlockType::Stone as u16;
		let inside = |v: i32, base: i32| (v - base).clamp(0, span);
		let idx = |x: i32, y: i32, z: i32| ((y * PADDED as i32 + z) * PADDED as i32 + x) as usize;
		// ONE reveal predicate, shared with the dig path: `nodes::reveal_met` is the authority
		// precisely so a second spelling here cannot answer differently about a block in neither
		// table. Pass two below reverts a claim on this answer, and a claim left standing that
		// the dig path calls plain stone is the worst outcome this whole system has.
		let hidden = |b: BlockType| !nodes::reveal_met(b, stamp.levels);
		// ---- the deposit stream (Stone hosts: the ore tiers + leystone). Runs deposit-major,
		// not cell-major: a chunk sits under a few dozen deposits and holds thousands of padded
		// cells, so walking the blobs is far less work than asking every cell. The lowest reveal
		// across the whole TIERS table still early-outs the walk for a player who has reached
		// none of them on either axis, which is mostly the fresh-account case.
		let any_deposit_visible = stamp.levels.mining >= ore::lowest_reveal()
			|| (stamp.levels.siphon && ore::siphon_reveals_deposit());
		if stamp.hosts.deposits && any_deposit_visible && oy <= ore::MAX_DEPOSIT_Y + ore::MAX_RADIUS
		{
			// Pass one: every deposit CLAIMS its cells, revealed or not, in the same grid order
			// `ore::ore_at` walks — so the first deposit to reach a shared cell wins on both
			// sides. A claim that turns out invisible is undone below; skipping unrevealed
			// deposits here would let a later one stamp a cell the dig path says belongs to the
			// first, and a node you can see and cannot extract is the worst outcome here.
			let mut claimed: Vec<u16> = Vec::new();
			for d in ore::deposits_near(
				(ox, oy, oz),
				(ox + span, oy + span, oz + span),
				stamp.seed.value,
			) {
				for y in inside(d.y - ore::MAX_RADIUS, oy)..=inside(d.y + ore::MAX_RADIUS, oy) {
					for z in inside(d.z - ore::MAX_RADIUS, oz)..=inside(d.z + ore::MAX_RADIUS, oz) {
						for x in
							inside(d.x - ore::MAX_RADIUS, ox)..=inside(d.x + ore::MAX_RADIUS, ox)
						{
							let i = idx(x, y, z);
							if self.blocks[i] != stone
								|| !d.contains(ox + x, oy + y, oz + z)
								|| stamp.edited.binary_search(&(i as u16)).is_ok()
							{
								continue;
							}
							self.blocks[i] = d.ore as u16;
							claimed.push(i as u16);
						}
					}
				}
			}
			// Pass two: anything claimed by a tier this looker cannot see
			// goes back to being stone, which is exactly what they dig.
			//
			// Only the CLAIMED indices, never a sweep of the array: a
			// PLACED block is ordinary chunk data that everyone can see at
			// any level, and a sweep would quietly turn somebody's trophy
			// wall into stone for every visitor below the tier.
			for i in claimed {
				let b = BlockType::from_id(self.blocks[i as usize]);
				if hidden(b) {
					self.blocks[i as usize] = stone;
				}
			}
		}
		// ---- heartwood (Wood hosts). Column-major: a trunk is a column,
		// and `trunk_at` is memoized per candidate cell, so the walk costs
		// a few hundred memo hits — and only for columns that hold Wood at
		// all, which prunes everything but forest.
		let wood = BlockType::Wood as u16;
		if stamp.hosts.heartwood && !hidden(BlockType::Heartwood) {
			for z in 0..=span {
				for x in 0..=span {
					if !(0..=span).any(|y| self.blocks[idx(x, y, z)] == wood) {
						continue;
					}
					let (wx, wz) = (ox + x, oz + z);
					let Some(t) = crate::decoration::trunk_at(wx, wz, stamp.seed) else {
						continue;
					};
					for wy in t.y0.max(oy)..=t.y1.min(oy + span) {
						let i = idx(x, wy - oy, z);
						if self.blocks[i] == wood
							&& stamp.edited.binary_search(&(i as u16)).is_err()
							&& nodes::heartwood_at(wx, wy, wz, stamp.seed)
						{
							self.blocks[i] = BlockType::Heartwood as u16;
						}
					}
				}
			}
		}
		// ---- springstone (Sand hosts). Column-major, like the heartwood pass above it: a bed
		// is a property of the COLUMN, so one `springstone_bed` answers for the whole of it and
		// only the y is left to check. It must NOT pre-filter on the cell above being water,
		// cheap as that is: that reads the SNAPSHOT where `nodes::revealed` reads worldgen, so a
		// predicted bucket scoop hid the spring while the dig path still called it a draw.
		let sand = BlockType::Sand as u16;
		if stamp.hosts.springs && !hidden(BlockType::Springstone) {
			for z in 0..=span {
				for x in 0..=span {
					// The host prefilter first, so an unsanded column
					// never reaches worldgen at all — the same shape the
					// heartwood pass uses, and cheap array reads either
					// way.
					if !(0..=span).any(|y| self.blocks[idx(x, y, z)] == sand) {
						continue;
					}
					let Some(bed) = nodes::springstone_bed(ox + x, oz + z, stamp.seed) else {
						continue;
					};
					let y = bed - oy;
					if !(0..=span).contains(&y) {
						continue;
					}
					let i = idx(x, y, z);
					if self.blocks[i] == sand && stamp.edited.binary_search(&(i as u16)).is_err() {
						self.blocks[i] = BlockType::Springstone as u16;
					}
				}
			}
		}
	}
}

pub struct MeshData {
	/// Packed two-word vertices (see `pack`), 4 per face (TL,TR,BL,BR).
	pub solid: Vec<u32>,
	/// Same two-word format; word 1 leaves the layer field zero and carries the water payload
	/// packed above it, in order: the surface drop code, the falling flag, the flow direction, the
	/// splash code (one lower per ring of Chebyshev distance out — expanding foam rings), and the
	/// vertex fall distance. Bottom-plane verts take one more block of fall so the interpolated
	/// "blocks fallen" is continuous down a column, and the field saturates rather than wrapping.
	pub water: Vec<u32>,
	/// Water-fall impact cells (the fall-impact predicate), packed
	/// chunk-local as x, then y, then z — spray-emitter metadata. Rides the
	/// mesh so every fall-changing path (worldgen stream-in, dig/place,
	/// fluid sim, remesh-all) refreshes emitters with no extra scans.
	pub impacts: Vec<u16>,
	/// The blended-glass stream: `solid`'s two-word format exactly, split
	/// out because it draws after water with no depth write, chunk-sorted
	/// back-to-front (its two blend halves do not commute across panes).
	/// Faces land here by BUCKET, so a shaped translucent (a crystal slab,
	/// a pane) rides along with the glass cubes.
	pub translucent: Vec<u32>,
	/// Exposed faces of SURFACE emitters, greedy-merged into rectangles ([`lights::pack`]) — the
	/// GI's area lights for revealed veins. Rides the mesh because the reveal has already run over
	/// this padded array (so the list holds only tiers this player can SEE) and face exposure is
	/// the question the mesher was already answering. Every path that changes either — streaming,
	/// a dig, a place, levelling into a new tier — re-meshes, so nothing has to be kept in step.
	pub lights: Vec<u32>,
}

impl MeshData {
	pub fn is_empty(&self) -> bool {
		self.solid.is_empty() && self.water.is_empty() && self.translucent.is_empty()
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
	// A see-through neighbor covers nothing, whatever its class — checked
	// by BUCKET before the class match, or a translucent SHAPED cell's
	// coverage mask would cull the very face you see through it.
	if b.props().bucket == crate::block::MeshBucket::Translucent {
		return Cover::None;
	}
	geometric_cover(cell, dir)
}

/// [`neighbor_cover`] without the see-through bypass: the coverage this cell presents as GEOMETRY,
/// ignoring whether you can see through it. Only same-type translucent neighbours ask for this — it
/// gives a shaped pane the interior-face rule `RenderClass::Glass` gives a full cube, and asking it
/// of a DIFFERENT translucent type would cull the face you see through.
fn geometric_cover(cell: u16, dir: Dir) -> Cover {
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
		// Same-type culling, glass edition: a window's interior faces
		// vanish while its faces toward air, water, shaped covers and
		// OTHER glass all draw.
		RenderClass::Glass => nb != me,
		// Water emits toward air and toward anything SEE-THROUGH, and
		// see-through is the BUCKET — a crystal slab keeps class
		// `Shaped`, so asking the class here left a water void behind
		// every translucent shaped cover. Toward an opaque Shaped
		// neighbour it still emits nothing: the known v1 artifact,
		// pinned by test — you can see through a stair's open region
		// into the void behind it.
		RenderClass::Translucent => {
			nb.props().class == RenderClass::None
				|| nb.props().bucket == crate::block::MeshBucket::Translucent
		}
		// Shaped cells never route here — mesh_chunk branches to the
		// shaped emitter first.
		RenderClass::Shaped => unreachable!("shaped cells use emit_shaped"),
	}
}

/// Pack a vertex's GEOMETRY word (word 0 — solid and water share it), in order: x, y and z in
/// TWELFTH-block units (the sub-block shape grid), then the normal, then the corner code. Word 1 is
/// per-stream material, assembled by the caller: [`solid_word1`] carries texture layer, uv in
/// twelfths, the emissive flag and tint-from-below; water leaves the layer field zero and carries
/// the payload above it. Every consumer moves with this layout — chunk.wgsl's `vs` and `vs_depth`,
/// water.wgsl, the attributes.
#[inline]
fn pack(pos: (u32, u32, u32), normal: Dir, corner: u32) -> u32 {
	let (x, y, z) = pos;
	debug_assert!(x <= 192 && y <= 192 && z <= 192 && corner < 4);
	x | (y << 8) | (z << 16) | ((normal as u32) << 24) | (corner << 27)
}

/// The solid stream's material word (see [`pack`] for the layout).
#[inline]
fn solid_word1(tex: u32, uv_u: u32, uv_v: u32, emissive: bool, tint_below: bool) -> u32 {
	debug_assert!((tex as usize) < crate::block::MAX_TEX_LAYERS);
	debug_assert!(uv_u <= 12 && uv_v <= 12);
	tex | (uv_u << 10) | (uv_v << 14) | ((emissive as u32) << 18) | ((tint_below as u32) << 19)
}

/// Corner lattice positions [TL, TR, BL, BR] of the face of block (x, y, z) toward `dir`, as a
/// per-face origin offset plus a diagonal vector. Public because this order and [`quad_indices`]
/// are one contract: any quad drawn through that index buffer must come out of this table, or it
/// winds inward and `cull: Back` deletes it. The render crate's item geometry builds on it.
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

/// Emit one face quad in `face_corners` order (TL TR BL BR), which [`quad_indices`] triangulates as
/// the diagonal between the first and last emit slots. With baked AO gone there is no per-corner
/// signal left to steer the diagonal, so the winding is simply fixed — pinned by
/// `every_quad_winds_in_face_corner_order`, since a quad wound the other way `cull: Back` deletes.
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
		// the shader used to derive itself. A full cube always tints from
		// its own cell — the only multi-cell object is the door, and a
		// door is Shaped.
		out.push(solid_word1(
			tex,
			(ci & 1) * 12,
			(ci >> 1) * 12,
			emissive,
			false,
		));
	}
}

/// Stamp a shaped block's baked quad template. Outlined like the water path — shaped cells are rare
/// and the solid loop is hot. Interior quads always emit; boundary quads cull against the
/// neighbor's coverage of the shared plane, mask-driven so stacked mirrored stairs and
/// stair-against-wall cull correctly, cross-chunk included since the padded shell carries state.
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
	// A door's upper half hashes the cell below it for its per-block tint,
	// so the two-cell leaf wears one tint. Per cell, not per quad.
	let tint_below = crate::shapes::tint_from_below(cell);
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
			let ncell = input.get_cell(bx + ox, by + oy, bz + oz);
			// Same-type culling, shaped edition. `neighbor_cover` answers `None` for any
			// translucent neighbour so a pane cannot cull the face you see through it — but
			// between two cells of the SAME type there is nothing to see, and emitting both
			// leaves coincident quads the no-depth-write glass pass tints twice. The state rides
			// along, so a top slab beside a bottom slab still draws: their shapes do not meet.
			let same_type = crate::chunk::cell_id(ncell) == me as u16;
			let cover = if same_type && me.props().bucket == crate::block::MeshBucket::Translucent {
				geometric_cover(ncell, dir)
			} else {
				neighbor_cover(ncell, dir)
			};
			match cover {
				Cover::Full => continue,
				Cover::Partial(m) if m.covers(q.rect[0], q.rect[1], q.rect[2], q.rect[3]) => {
					continue;
				}
				_ => {}
			}
		}
		let tex = q.tex.map_or_else(|| me.texture(q.dir), u32::from);
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
				tint_below,
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
		translucent: Vec::new(),
		impacts: Vec::new(),
		lights: lights::collect(input),
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
					// Stream selection is by BUCKET here and below — a
					// translucent shaped row keeps its class and still
					// lands in the blended stream.
					let out = if me.props().bucket == crate::block::MeshBucket::Translucent {
						&mut mesh.translucent
					} else {
						&mut mesh.solid
					};
					emit_shaped(out, input, (x, y, z), me, cell);
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
					match me.props().bucket {
						crate::block::MeshBucket::Water => {
							let state = *wstate.get_or_insert_with(|| {
								WaterState::of(input, &mut falls, x as i32, y as i32, z as i32, me)
							});
							emit_water_face(&mut mesh.water, input, (x, y, z), dir, state);
						}
						crate::block::MeshBucket::Translucent => {
							emit_face(
								&mut mesh.translucent,
								(x, y, z),
								dir,
								tex,
								me.props().emissive,
							);
						}
						crate::block::MeshBucket::Solid => {
							emit_face(&mut mesh.solid, (x, y, z), dir, tex, me.props().emissive);
						}
					}
				}
			}
		}
	}
	mesh
}

/// The static index pattern shared by every chunk mesh: each quad's four
/// vertices, triangulated in the Java order (BR,TR,TL)+(BR,TL,BL), which
/// winds CCW.
pub fn quad_indices(max_quads: usize) -> Vec<u32> {
	let mut idx = Vec::with_capacity(max_quads * 6);
	for q in 0..max_quads as u32 {
		let b = q * 4;
		idx.extend_from_slice(&[b + 3, b + 1, b, b + 3, b, b + 2]);
	}
	idx
}

#[cfg(test)]
mod tests;
