//! GI-class classification for the RTGI voxel window.
//!
//! The window is an `Rg8Uint` 3D texture, one texel per block, that the probe
//! rays march. Classification is a pure function of the cell, so every consumer
//! agrees by construction. A texel is TWO palette indices, each deduplicated
//! over the roster so it grows with what is DISTINCT rather than with the block
//! count: `.r` is the cell's LOOK (everything a ray needs to shade or pass a
//! cell — albedo texture, cast light, opacity — the render side's material LUT
//! is one row per look) and `.g` is its COVER (the occlusion of a shaped cell:
//! the exact twelfths raster of `shapes::light_boxes`, plus the box bounding it
//! for the marches' cheap reject). Cover 0 is a full cell; look 0 is air. A
//! shaped cell shares its cube's look, so a slab in a new material costs no
//! look and no cover, and a state with no light boxes — an open door —
//! classifies as air. The top `PREVIEW_COVER_SLOTS` covers are the shape
//! editor's, rewritten live so a sculpt in progress occludes as itself.

use std::sync::LazyLock;

use crate::block::{BlockType, RenderClass};
use crate::chunk::{CHUNK_SIZE, CHUNK_VOLUME, ChunkData};
use crate::shapes::{self, SubBox};

/// What a ray does when it enters a cell of this class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GiOpacity {
	Air,
	Translucent,
	Opaque,
	/// See-through with per-class rgb transmittance: the marches multiply throughput, never stop.
	Tinted,
}

/// Bytes per window texel: the look, then the cover.
pub const BYTES_PER_CELL: usize = 2;

/// One chunk's worth of window texels.
pub const CHUNK_BYTES: usize = CHUNK_VOLUME * BYTES_PER_CELL;

/// One classified chunk, ready for the GPU: texel bytes in
/// write_texture order, the coarse brick words, and the cells
/// that emit (for the light list).
pub struct GiChunk {
	/// `[z][y][x]` — depth slices in `write_texture` layout, x fastest
	/// (NOT ChunkData's y-major order), [`BYTES_PER_CELL`] per texel.
	pub bytes: Box<[u8; CHUNK_BYTES]>,
	/// One [`pack_coarse`] word per brick of the chunk, `[z][y][x]`.
	pub coarse: [u32; 8],
	/// Emitting cells.
	pub emitters: Vec<GiEmitter>,
}

/// One point emitter in a chunk: its local cell and both classes — the look is what it casts,
/// the cover is where inside the cell it casts FROM ([`light_origin`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GiEmitter {
	pub at: [u8; 3],
	pub look: u8,
	pub cover: u8,
}

/// Cells along a brick's edge; a brick address is a cell address shifted by its log.
pub const BRICK: usize = 8;
const BRICK_CELLS: usize = BRICK * BRICK * BRICK;

/// A brick's far-field summary (R41): a heightfield with a density. Each column of
/// the brick has a TOP, one above its highest blocker (zero for none); `lo` and `hi` are the
/// lowest and highest tops. A ray segment above `hi` is open, one below `lo` crosses the CORE at
/// `core` density, and the band between crosses at `band` density — so a ray skimming flat ground
/// inside the ground's own brick is open, which one density over the whole brick got wrong.
/// `modal` is the look most blockers wear. Densities are fractions of their full scale —
/// `CORE_FULL` and `BAND_FULL`, the two bit widths the word affords — and any brick with a
/// blocker has `hi > 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Brick {
	pub lo: u8,
	pub hi: u8,
	pub core: u8,
	pub band: u8,
	pub modal: u8,
}

pub const CORE_FULL: u8 = 63;
pub const BAND_FULL: u8 = 255;

/// A brick's coarse word: two flags (any ray-blocking cell, any emitting cell) that alone decide
/// whether a march may SKIP the brick, then the [`Brick`] summary a far gather ray reads to cross
/// the brick without descending into it. A brick with any blocker has a nonzero summary, so "word
/// is zero" and "nothing to march" stay one test. `gi_common.wgsl` and `gi_probe.wgsl` decode the
/// same layout.
///
/// The summary's `hi` looked like an exact skip for the cell-level marches too (a segment above
/// every column top crosses no blocker), and it was built and MEASURED OUT on the stare bench:
/// the per-cell compare it adds to the latency-bound loop cost more than the surface-band crawl
/// it saved, and water — which raises no top — needed a flag bit to veto it. Left out.
pub fn pack_coarse(blocks_rays: bool, emits: bool, b: Brick) -> u32 {
	u32::from(blocks_rays)
		| u32::from(emits) << 1
		| u32::from(b.core) << 2
		| u32::from(b.band) << 8
		| u32::from(b.modal) << 16
		| u32::from(b.lo) << 24
		| u32::from(b.hi) << 28
}

/// The summary a coarse word carries.
pub fn unpack_coarse(word: u32) -> Brick {
	Brick {
		lo: ((word >> 24) & 0xf) as u8,
		hi: (word >> 28) as u8,
		core: ((word >> 2) & 0x3f) as u8,
		band: (word >> 8) as u8,
		modal: (word >> 16) as u8,
	}
}

/// A density as the word stores it: a fraction of `cells` on the scale `full`, never zero for a
/// nonzero count (so a lone blocker still makes its brick a medium rather than open air).
fn density(blockers: usize, cells: usize, full: u8) -> u8 {
	match cells {
		0 => 0,
		_ => (blockers * usize::from(full))
			.div_ceil(cells)
			.min(usize::from(full)) as u8,
	}
}

/// Summarize one brick from its blocker mask, `[z][y][x]` over the brick's own cells.
fn summarize_brick(blk: &[bool; BRICK_CELLS], hist: &[u16; 256]) -> Brick {
	let at = |x: usize, y: usize, z: usize| blk[(z * BRICK + y) * BRICK + x];
	let (mut lo, mut hi) = (BRICK, 0);
	for z in 0..BRICK {
		for x in 0..BRICK {
			let top = (0..BRICK).rev().find(|&y| at(x, y, z)).map_or(0, |y| y + 1);
			lo = lo.min(top);
			hi = hi.max(top);
		}
	}
	let count = |y0: usize, y1: usize| {
		(y0..y1)
			.flat_map(|y| (0..BRICK).flat_map(move |z| (0..BRICK).map(move |x| (x, y, z))))
			.filter(|&(x, y, z)| at(x, y, z))
			.count()
	};
	let modal = match hi {
		0 => 0,
		_ => (0..256).max_by_key(|&c| hist[c]).unwrap_or(0) as u8,
	};
	Brick {
		lo: lo as u8,
		hi: hi as u8,
		core: density(count(0, lo), lo * BRICK * BRICK, CORE_FULL),
		band: density(count(lo, hi), (hi - lo) * BRICK * BRICK, BAND_FULL),
		modal,
	}
}

/// How many classes a palette byte holds, index 0 reserved (air, or no
/// cover). Both palettes are one byte wide because the window is: a
/// wider texel doubles the frame's largest allocation again.
pub const PALETTE: usize = 256;

/// The margin each palette holds back, so a catalog wave is priced before
/// it is written rather than after the `LazyLock` panics at startup.
pub const PALETTE_MARGIN: usize = 32;

/// One look: the block whose row the material LUT builds this class from.
/// Every block with the same [`look_key`] shares it, so the LUT row is the
/// same whichever member is named.
pub struct Look {
	pub base: BlockType,
}

/// What makes two blocks one look: the texture the LUT's albedo is taken from, what it casts,
/// and how a ray crosses it. Everything the material LUT reads and nothing it does not, so a
/// row built from any member of the class is bit-identical.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LookKey {
	tex: u16,
	light: u8,
	flecks: Option<([u8; 3], [u8; 3])>,
	op: GiOpacity,
}

fn look_key(b: BlockType) -> LookKey {
	let p = b.props();
	LookKey {
		tex: p.tex_top,
		light: p.light,
		flecks: b.node_flecks(),
		op: opacity(b),
	}
}

/// The look palette and the id → look index.
static LOOKS: LazyLock<(Vec<Look>, Vec<u8>)> = LazyLock::new(|| {
	let mut looks: Vec<Look> = Vec::new();
	let mut keys = Vec::new();
	let mut index = vec![0u8; BlockType::COUNT];
	for b in BlockType::ALL {
		if b == BlockType::Air {
			continue;
		}
		let key = look_key(b);
		let found = keys.iter().position(|k| *k == key).unwrap_or_else(|| {
			looks.push(Look { base: b });
			keys.push(key);
			looks.len() - 1
		});
		let class = 1 + found;
		assert!(class < PALETTE, "looks overflow the palette byte");
		index[b as usize] = class as u8;
	}
	(looks, index)
});

/// Every look, indexed by `look - 1`. The render side builds the LUT's
/// rows from 1 up out of this; row 0 is air.
pub fn looks() -> &'static [Look] {
	&LOOKS.0
}

/// Words in one cover's occupancy mask: every twelfth of the cell, one bit each.
pub const COVER_WORDS: usize = (shapes::GRID as usize).pow(3).div_ceil(32);

/// Words per cover-table entry: the packed bounds, a pad word that keeps
/// the mask at word 2, then the mask. The GPU table's stride.
pub const COVER_ENTRY_WORDS: usize = 2 + COVER_WORDS;

/// One cover: the exact twelfths occupancy of a shaped cell's light boxes,
/// and the box bounding it. Bit `shapes::occ_idx(x, y, z)` of the mask
/// (word `i >> 5`, bit `i & 31`) is a filled twelfth — gi_common's
/// `gi_cover_march` decodes this layout, pinned by
/// `mask_bits_match_the_shader_decode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cover {
	pub bounds: SubBox,
	pub mask: [u32; COVER_WORDS],
}

impl Cover {
	/// The cover of a light-box list; `None` for an empty one, which is
	/// no cover at all (an open door classifies as air).
	pub fn from_light_boxes(boxes: &[SubBox]) -> Option<Cover> {
		let bounds = union_box(boxes)?;
		let mut mask = [0u32; COVER_WORDS];
		for (i, &filled) in shapes::rasterize(boxes).iter().enumerate() {
			if filled {
				mask[i >> 5] |= 1 << (i & 31);
			}
		}
		Some(Cover { bounds, mask })
	}

	/// Whether twelfth (x, y, z) is filled.
	pub fn filled(&self, x: u8, y: u8, z: u8) -> bool {
		let i = shapes::occ_idx(i32::from(x), i32::from(y), i32::from(z));
		(self.mask[i >> 5] >> (i & 31)) & 1 != 0
	}
}

/// Cover slots the shape editor rewrites live, one per footprint part, at
/// the top of the palette: the palette is built once from the shipped
/// roster, and a sculpt in progress must occlude as itself, not as the
/// block whose registry entry it borrows.
pub const PREVIEW_COVER_SLOTS: usize = shapes::def::MAX_PARTS as usize;

/// The cover byte a preview part classifies to (`gi_classes`), and the
/// table entry the editor writes that part's cover into.
pub fn preview_cover_slot(part: u8) -> u8 {
	(PALETTE - 1 - usize::from(part) % PREVIEW_COVER_SLOTS) as u8
}

fn union_box(boxes: &[SubBox]) -> Option<SubBox> {
	let mut it = boxes.iter();
	let first = *it.next()?;
	Some(it.fold(first, |a, b| SubBox {
		min: [
			a.min[0].min(b.min[0]),
			a.min[1].min(b.min[1]),
			a.min[2].min(b.min[2]),
		],
		max: [
			a.max[0].max(b.max[0]),
			a.max[1].max(b.max[1]),
			a.max[2].max(b.max[2]),
		],
	}))
}

/// The cover palette and the (id, state) → cover index. Index slots hold the cover class, or 0
/// for "occludes as its whole cell" — a full cube, a translucent shaped cell (a crystal slab, a
/// pane: the GI approximates it as a full see-through cell, since a box that attenuates instead
/// of blocking has no home in the box words) — and for an open door, whose look is air anyway.
/// Deduplicated over the GEOMETRY alone: every material's floor slab is one cover.
static COVERS: LazyLock<(Vec<Cover>, Vec<u8>)> = LazyLock::new(|| {
	let mut covers: Vec<Cover> = Vec::new();
	let mut index = vec![0u8; BlockType::COUNT * 16];
	for b in BlockType::ALL {
		if b.props().shape.is_none() || b.props().bucket == crate::block::MeshBucket::Translucent {
			continue;
		}
		// All 16 nibbles, not just canonically-valid states: `shape_of`
		// masks a nibble down to a real variant, and the window must
		// answer for whatever a journal or an edit actually stored.
		for state in 0..16u8 {
			let cell = crate::chunk::make_cell(b, state);
			let key = ((b as usize) << 4) | state as usize;
			let Some(cover) = Cover::from_light_boxes(shapes::light_boxes(cell)) else {
				index[key] = 0;
				continue;
			};
			let found = covers.iter().position(|c| *c == cover).unwrap_or_else(|| {
				covers.push(cover);
				covers.len() - 1
			});
			let class = 1 + found;
			assert!(
				class < PALETTE - PREVIEW_COVER_SLOTS,
				"covers overflow the palette byte"
			);
			index[key] = class as u8;
		}
	}
	(covers, index)
});

/// Every cover, indexed by `cover - 1`. The render side packs these into
/// the cover table from 1 up; entry 0 is the no-box sentinel.
pub fn covers() -> &'static [Cover] {
	&COVERS.0
}

/// The emissive band of a shaped emitter, as a fraction of its cell measured DOWN FROM THE CELL
/// TOP. texgen's `torch_side` paints its top rows as fire and leaves the rest as wood — this
/// constant is that band — and shaped UVs are cut from the CELL's face (`shapes::uv_cell`), so the
/// band is cell-relative, not box-relative: the part above the shape's own top isn't drawn. For
/// the torch, whose post stops short of the cell top, that leaves a flame over the post.
const EMISSIVE_BAND: f32 = 5.0 / 12.0;

/// Where inside its cell an emitter of this COVER actually radiates from, in block units (0..1
/// per axis). A full cube (cover 0) emits from its center; a shaped emitter does not — a torch is
/// a slim post whose fire is the top of its height, so a center origin puts the point source
/// down in the handle. Under `LIGHT_NEAR` that costs no brightness, but the hard point shadows
/// project from the wrong height. Written per-axis though only y moves today.
pub fn light_origin(cover: u8) -> [f32; 3] {
	const MID: [f32; 3] = [0.5, 0.5, 0.5];
	if cover == 0 {
		return MID;
	}
	let Some(c) = covers().get(usize::from(cover) - 1) else {
		return MID;
	};
	let b = c.bounds;
	let g = f32::from(shapes::GRID);
	let ctr = |lo: u8, hi: u8| (f32::from(lo) + f32::from(hi)) * 0.5 / g;
	// Clip the cell's emissive band to the shape's own extent. A shape
	// that doesn't reach the band at all (a slab, were one ever to emit)
	// keeps its box center rather than an origin outside its geometry.
	let band_lo = (1.0 - EMISSIVE_BAND) * g;
	let y = if f32::from(b.max[1]) > band_lo {
		(band_lo.max(f32::from(b.min[1])) + f32::from(b.max[1])) * 0.5 / g
	} else {
		ctr(b.min[1], b.max[1])
	};
	[ctr(b.min[0], b.max[0]), y, ctr(b.min[2], b.max[2])]
}

/// How far below the eye a light a player is CARRYING hangs. The eye sits
/// [`crate::player::EYE_TO_CROWN`] below the crown of a [`crate::player::HEIGHT`] body, so this
/// hangs around shoulder height — comfortably inside the capsule, which is the whole point. The
/// crown is the near wall: shrinking this drop thins the overlap between the light's cell and the
/// body that proves that cell non-solid, pinned by `a_carried_light_hangs_inside_the_body`.
pub const CARRY_DROP: f32 = 0.25;

/// Where a light carried by a player whose eye is at `eye` radiates from. Deliberately a function
/// of POSITION alone and not of facing: the light does not sit where the item is drawn, about half
/// a block off, which against `LIGHT_RANGE` is nothing. What it buys — **mouse-look never moves
/// the light**, so a player turning in place is not a moving emitter and the resolve's history
/// climbs back; and **the cell is guaranteed non-solid**, the player's own capsule occupies it.
pub fn carried_origin(eye: glam::Vec3) -> glam::Vec3 {
	eye - glam::Vec3::Y * CARRY_DROP
}

/// What a held emitter casts at, for a wielder at this Mining level. Every block answers with its
/// own row and stops there — a torch in a hand is the torch on the floor (R28). The Resonant Pick
/// is the one exception, keyed on [`crate::unlock::pick_cells`] reaching
/// [`crate::unlock::PICK_PLANE`] rather than a level of its own, because two thresholds for one
/// rank drift apart. A remote's pick casts at its row's level — their skills are not in a snapshot.
pub fn carried_light(b: BlockType, mining: u32) -> CarriedLight {
	if b == BlockType::ResonantPick
		&& crate::unlock::pick_cells(mining) == crate::unlock::PICK_PLANE
	{
		return CarriedLight {
			level: PICK_RESONANT_LIGHT,
			whitened: true,
		};
	}
	CarriedLight {
		level: b.props().light,
		whitened: false,
	}
}

/// What a held emitter casts: how much, and in what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CarriedLight {
	/// The cast level, [`crate::block::BlockProps::light`]'s units.
	pub level: u8,
	/// Draw this light's tint pulled toward white. The pick's authored blue is what makes its glow
	/// READ as resonance, and also why the tool is a poor lamp: a saturated blue carries almost no
	/// luminance. At the resonant rank the tool stops being a signal and becomes a light to work
	/// by — brighter, and less blue for the same reason a hot flame is less orange. It rides the
	/// light-list ENTRY beside the level, not the class, because it is a property of the wielder.
	pub whitened: bool,
}

/// What the Resonant Pick casts at once it breaks the full plane — see [`carried_light`], the only
/// thing that reads it. Set against the WHITENED tint rather than the authored one, which is most
/// of the answer to why it is not higher: whitening more than doubles what the same level
/// delivers, so this lands the resonant pick as the brightest thing a hand can hold short of a
/// torch, and the step up from its own first rank is one a player cannot miss.
const PICK_RESONANT_LIGHT: u8 = 12;

/// Both classes of one cell, as the window texel carries them. A full cube is its look and no
/// cover. A shaped cell wears its cube's look with the state's cover — the one place the state
/// nibble matters (a thin floor slab and a thick ceiling slab occlude differently) — EXCEPT that
/// a shaped cell with no light boxes at all (an open door, the editor's empty preview) is AIR on
/// both counts: the two classes are read independently on the GPU, and a look with no cover is
/// a whole opaque cell, which is exactly what a doorway must never be. A translucent shaped cell
/// keeps its look and carries no cover: the GI takes it as a full see-through cell. A cell the
/// shape editor is previewing through answers its part's rewritable slot (`preview_cover_slot`).
#[inline]
pub fn gi_classes(cell: u16) -> (u8, u8) {
	let b = BlockType::from_id(crate::chunk::cell_id(cell));
	let look = LOOKS.1[b as usize];
	let p = b.props();
	if p.shape.is_none() || p.bucket == crate::block::MeshBucket::Translucent {
		return (look, 0);
	}
	let state = crate::chunk::cell_state(cell);
	if shapes::preview_active(b, state) {
		return (look, preview_cover_slot(state));
	}
	match COVERS.1[((b as usize) << 4) | state as usize] {
		0 => (0, 0),
		cover => (look, cover),
	}
}

/// The look of one cell: 0 for air, else the block's look class.
#[inline]
pub fn gi_look(cell: u16) -> u8 {
	gi_classes(cell).0
}

/// The cover of one cell: 0 for a full cell, else a [`covers`] entry.
#[inline]
pub fn gi_cover(cell: u16) -> u8 {
	gi_classes(cell).1
}

/// How a look attenuates rays (LUT-side mirror; also the coarse rule). A covered cell does not
/// route through this — its cover is partial by construction — EXCEPT a translucent one, which
/// carries no cover and answers as its whole cell. The see-through test is the BUCKET, not a
/// class: a translucent shaped row keeps `RenderClass::Shaped`, and `Translucent`-the-class still
/// means water (the marches' op-1 path-length counter).
#[inline]
pub fn opacity(b: BlockType) -> GiOpacity {
	if b.props().bucket == crate::block::MeshBucket::Translucent {
		return GiOpacity::Tinted;
	}
	match b.props().class {
		RenderClass::None => GiOpacity::Air,
		RenderClass::Translucent => GiOpacity::Translucent,
		_ => GiOpacity::Opaque,
	}
}

/// Pack one sub-box into the cover table's bounds word: the three min coordinates
/// then the three max, a nibble each (a sub-grid coordinate fits). 0 is
/// the "no box" sentinel — a real box always has a nonzero max. The WGSL
/// decode in gi_dda.wgsl's `gi_box_span` is this formula's mirror,
/// pinned by `box_pack_matches_the_shader_decode`.
#[inline]
pub fn pack_box(b: SubBox) -> u32 {
	u32::from(b.min[0])
		| u32::from(b.min[1]) << 4
		| u32::from(b.min[2]) << 8
		| u32::from(b.max[0]) << 12
		| u32::from(b.max[1]) << 16
		| u32::from(b.max[2]) << 20
}

/// Classify a whole chunk. Air chunks classify to all-zero bytes —
/// callers still upload them, because the window is toroidal and the
/// region may hold a stale previous tenant.
pub fn classify_chunk(data: &ChunkData) -> GiChunk {
	let n = CHUNK_SIZE;
	let mut bytes = Box::new([0u8; CHUNK_BYTES]);
	let mut flags = [0u32; 8];
	let mut hist = [[0u16; 256]; 8];
	let mut blk = Box::new([[false; BRICK_CELLS]; 8]);
	let mut emitters = Vec::new();
	for z in 0..n {
		for y in 0..n {
			for x in 0..n {
				let cell = data.get_cell(x, y, z);
				let (look, cover) = gi_classes(cell);
				let i = ((z * n + y) * n + x) * BYTES_PER_CELL;
				bytes[i] = look;
				bytes[i + 1] = cover;
				if look == 0 {
					continue;
				}
				let b = BlockType::from_id(crate::chunk::cell_id(cell));
				// A SURFACE emitter is not a point source and does not belong in this
				// list: a vein radiates from the faces it opens into, and most of a vein
				// opens into nothing. The mesher — already deciding face by face what is
				// exposed — hands the renderer those rectangles instead (R31). A point
				// here as well would light buried rock AND double every cell.
				let emits = b.props().light > 0 && !b.props().surface_light;
				if emits {
					emitters.push(GiEmitter {
						at: [x as u8, y as u8, z as u8],
						look,
						cover,
					});
				}
				// A partial cell sets the blocker bit too: the coarse
				// texture only licenses SKIPPING a brick, and a brick
				// holding a slab must still be marched. Tinted cells set
				// it for the same reason — a skipped brick accumulates no
				// throughput, so a wall of panes would go untinted.
				let blocks_rays =
					cover != 0 || matches!(opacity(b), GiOpacity::Opaque | GiOpacity::Tinted);
				let ci = (z / BRICK) * 4 + (y / BRICK) * 2 + (x / BRICK);
				flags[ci] |= u32::from(blocks_rays) | u32::from(emits) << 1;
				if blocks_rays {
					hist[ci][usize::from(look)] += 1;
					blk[ci][((z % BRICK) * BRICK + y % BRICK) * BRICK + x % BRICK] = true;
				}
			}
		}
	}
	let mut coarse = [0u32; 8];
	for ci in 0..8 {
		let b = summarize_brick(&blk[ci], &hist[ci]);
		coarse[ci] = pack_coarse(flags[ci] & 1 != 0, flags[ci] & 2 != 0, b);
	}
	GiChunk {
		bytes,
		coarse,
		emitters,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::chunk::make_cell;
	use crate::shapes::{Facing, door_state, slab_state};

	#[test]
	fn a_torch_radiates_from_its_flame_not_its_handle() {
		let o = light_origin(gi_cover(make_cell(BlockType::Torch, 0)));
		// The cell-relative emissive band clipped to the post leaves the
		// flame at the top of the post, and the origin is the middle of
		// what is left. Horizontally the centered post agrees with the cell
		// center.
		assert!((o[0] - 0.5).abs() < 1e-6, "{o:?}");
		assert!((o[2] - 0.5).abs() < 1e-6, "{o:?}");
		assert!((o[1] - 8.5 / 12.0).abs() < 1e-6, "{o:?}");
		// Full cubes are untouched — the point source stays the center.
		assert_eq!(
			light_origin(gi_cover(BlockType::Stone as u16)),
			[0.5, 0.5, 0.5]
		);
	}

	#[test]
	fn a_carried_light_hangs_inside_the_body() {
		use crate::player::{EYE_TO_CROWN, EYE_TO_FEET};
		// Yaw-independent: the origin is a function of position alone, so
		// a player turning on the spot is not a moving emitter. There is
		// no facing argument to pass — that IS the guarantee — so the
		// test states the consequence: only y moves.
		let eye = glam::Vec3::new(12.25, 70.5, -3.75);
		let o = carried_origin(eye);
		assert_eq!((o.x, o.z), (eye.x, eye.z));
		// Strictly inside the capsule the player already occupies, and
		// with clearance either side — that is what makes the light's
		// cell provably non-solid. Brushing a wall must never bury it and
		// put the room out.
		assert!(o.y + 0.5 < eye.y + EYE_TO_CROWN, "{o:?} near the crown");
		assert!(o.y - 0.5 > eye.y - EYE_TO_FEET, "{o:?} near the feet");
	}

	/// Both palettes and the headroom left in each — the numbers, not just
	/// a pass, because a catalog wave should be priced before it is written
	/// rather than after the `LazyLock` panics at startup. A look is spent
	/// per distinct TEXTURE-and-light, a cover per distinct GEOMETRY, and
	/// neither per block: that is the whole point of the split.
	#[test]
	fn the_palettes_fit_their_bytes_with_room_to_spare() {
		let room = PALETTE - 1;
		let n_looks = looks().len();
		let n_covers = covers().len();
		assert!(n_looks > 0 && n_covers > 0);
		assert!(
			n_looks + PALETTE_MARGIN <= room,
			"{n_looks} looks in {room} slots — only {} left",
			room - n_looks
		);
		let cover_room = room - PREVIEW_COVER_SLOTS;
		assert!(
			n_covers + PALETTE_MARGIN <= cover_room,
			"{n_covers} covers in {cover_room} slots — only {} left",
			cover_room - n_covers
		);
		// A shaped row in an existing material is free on both counts:
		// every slab shares its cube's look and the slab family's covers.
		assert_eq!(
			gi_look(make_cell(BlockType::WoodSlab, 0)),
			gi_look(BlockType::Wood as u16)
		);
		for state in 0..16u8 {
			assert_eq!(
				gi_cover(make_cell(BlockType::WoodSlab, state)),
				gi_cover(make_cell(BlockType::StoneSlab, state)),
				"state {state}: one geometry, one cover"
			);
		}
		// And the palettes are what the roster spends, spelled out.
		let blocks = BlockType::ALL.len() - 1;
		assert!(
			n_looks < blocks,
			"{n_looks} looks for {blocks} blocks: nothing deduplicated"
		);
	}

	#[test]
	fn a_look_is_everything_the_lut_reads_and_nothing_else() {
		// Every block answers a look, and two blocks share one exactly
		// when the LUT would build the same row from either.
		for a in BlockType::ALL {
			if a == BlockType::Air {
				assert_eq!(gi_look(a as u16), 0);
				continue;
			}
			assert_ne!(gi_look(a as u16), 0, "{a:?}");
			for b in BlockType::ALL {
				if b == BlockType::Air {
					continue;
				}
				assert_eq!(
					gi_look(a as u16) == gi_look(b as u16),
					look_key(a) == look_key(b),
					"{a:?} vs {b:?}"
				);
			}
			// The base named for a look really is a member of it.
			let l = &looks()[usize::from(gi_look(a as u16)) - 1];
			assert_eq!(look_key(l.base), look_key(a), "{a:?}");
		}
	}

	#[test]
	fn shaped_cells_get_covers_per_state() {
		// A floor slab and a ceiling slab occlude differently, so they
		// must classify differently — the old id-only rule collapsed
		// them (and everything else) into one opaque cube.
		let floor = gi_cover(make_cell(BlockType::WoodSlab, slab_state(false, false)));
		let ceil = gi_cover(make_cell(BlockType::WoodSlab, slab_state(false, true)));
		assert_ne!(floor, 0);
		assert_ne!(ceil, 0);
		assert_ne!(floor, ceil);
		// Stairs rotate: each facing is its own cover.
		let e = gi_cover(make_cell(BlockType::WoodStairs, Facing::PosX as u8));
		let w = gi_cover(make_cell(BlockType::WoodStairs, Facing::NegX as u8));
		assert_ne!(e, w);
		// A full cube has none.
		assert_eq!(gi_cover(BlockType::Stone as u16), 0);
	}

	#[test]
	fn an_open_door_lets_light_through() {
		// Open doors have no collision, so they must occlude nothing —
		// the doorway is the whole point of opening one.
		for f in Facing::ALL {
			for upper in [false, true] {
				let open = make_cell(BlockType::LumberDoor, door_state(f, true, upper));
				// Air on BOTH counts: a look with no cover would be a whole
				// opaque cell, and the doorway is the whole point.
				assert_eq!(gi_classes(open), (0, 0));
				let closed = make_cell(BlockType::LumberDoor, door_state(f, false, upper));
				assert_ne!(gi_cover(closed), 0);
				assert_eq!(gi_look(closed), gi_look(BlockType::Lumber as u16));
			}
		}
	}

	/// The see-through predicate is the BUCKET, swept over every block:
	/// a translucent row reads as tinted transmission to the rays, water
	/// alone reads as the water counter, and everything solid blocks. Derived from the same fields the table asserts, so
	/// a new row lands in exactly one arm or fails here by name.
	#[test]
	fn the_bucket_is_the_see_through_predicate() {
		use crate::block::{MeshBucket, RenderClass};
		// Set equality, both directions, rather than a re-spelling of `opacity`'s arms: the
		// bucket names EXACTLY the tinted blocks, and `Translucent`-the-class still names
		// exactly the water the marches count path length through. Written as memberships, a
		// row that gained the bucket without the op — or the reverse — fails here by name,
		// while a retune of the arms' ORDER does not.
		let (mut tinted, mut water) = (0u32, 0u32);
		for b in BlockType::ALL {
			let p = b.props();
			assert_eq!(
				p.bucket == MeshBucket::Translucent,
				opacity(b) == GiOpacity::Tinted,
				"{b:?}: the bucket and the tinted op name one set"
			);
			// Water is the OTHER see-through thing, and it must not have
			// drifted into the tinted op: it is a participating medium
			// with a path length, not a filter product.
			assert_eq!(
				p.class == RenderClass::Translucent && p.bucket != MeshBucket::Translucent,
				opacity(b) == GiOpacity::Translucent,
				"{b:?}: the water op stays the water class"
			);
			tinted += u32::from(opacity(b) == GiOpacity::Tinted);
			water += u32::from(opacity(b) == GiOpacity::Translucent);
		}
		// Neither sweep may pass by examining nothing.
		assert!(tinted > 0, "the bucket has members to sweep");
		assert!(water > 0, "the water class has members to sweep");
	}

	#[test]
	fn the_cover_is_the_exact_raster_of_the_light_boxes() {
		// Bit for bit: a twelfth is filled in the mask exactly when a
		// light box covers it (the two-box cover this replaced rounded a
		// 29-box stair up to its whole cell), and the bounds hold every
		// box, so the shader's cheap reject never rejects a hit.
		for b in BlockType::ALL {
			if b.props().shape.is_none() {
				continue;
			}
			for state in 0..16u8 {
				let cell = make_cell(b, state);
				let cover = gi_cover(cell);
				let boxes = shapes::light_boxes(cell);
				// A translucent shaped block carries no cover on purpose:
				// the GI takes it as a full see-through cell of its look,
				// boxes and all (see `COVERS`).
				if b.props().bucket == crate::block::MeshBucket::Translucent {
					assert_eq!(cover, 0, "{b:?}");
					continue;
				}
				if boxes.is_empty() {
					assert_eq!(cover, 0);
					continue;
				}
				let c = &covers()[usize::from(cover) - 1];
				let occ = shapes::rasterize(boxes);
				for (i, &filled) in occ.iter().enumerate() {
					let (x, y, z) = ((i % 12) as u8, (i / 144) as u8, ((i / 12) % 12) as u8);
					assert_eq!(
						c.filled(x, y, z),
						filled,
						"{b:?} state {state} at {x},{y},{z}"
					);
				}
				for cb in boxes {
					for a in 0..3 {
						assert!(c.bounds.min[a] <= cb.min[a] && c.bounds.max[a] >= cb.max[a]);
					}
				}
			}
		}
	}

	#[test]
	fn mask_bits_match_the_shader_decode() {
		// gi_common's `gi_cover_march` reads bit `(y*12 + z)*12 + x` of the
		// mask, word `i >> 5`, bit `i & 31` — `shapes::occ_idx`'s order.
		let c = Cover::from_light_boxes(&[SubBox::new([3, 5, 7], [4, 6, 8])]).unwrap();
		let i = (5 * 12 + 7) * 12 + 3;
		assert_eq!(c.mask[i >> 5], 1 << (i & 31));
		assert_eq!(c.mask.iter().filter(|&&w| w != 0).count(), 1);
		assert!(c.filled(3, 5, 7));
		assert!(!c.filled(4, 5, 7));
	}

	#[test]
	fn lumber_stairs_are_open_under_their_treads() {
		// The bug this file's masks fixed: thin treads on stringers used to
		// occlude as a solid cube. Tread rows are one twelfth thick with air
		// between them, and the stringers do not fill the cell's width.
		let c = &covers()[usize::from(gi_cover(make_cell(BlockType::LumberStairs, 0))) - 1];
		assert_ne!(c.mask, [u32::MAX; COVER_WORDS], "not a solid cube");
		// The first tread (x 0..4) at y 3..4, air directly under it at
		// mid-width, and the whole tread row is exactly one twelfth.
		assert!(c.filled(1, 3, 6));
		assert!(!c.filled(1, 2, 6));
		assert!(!c.filled(1, 4, 6));
		// Between the stringers (z 2..3 and 9..10) under the tread: open.
		assert!(!c.filled(1, 1, 6));
		assert!(c.filled(1, 1, 2), "the stringer foot");
		// A full cube's shadow it is not: fewer than half the twelfths.
		let filled: u32 = c.mask.iter().map(|w| w.count_ones()).sum();
		assert!(filled * 2 < 1728, "{filled} of 1728 twelfths filled");
	}

	/// The ramp pass draws a run of twelfth steps as one plane through each
	/// tread's low edge, so the drawn diagonal lies on the AIR side of the
	/// raster it merged: a point on a ramp never starts inside its own
	/// occluder, which is what lets the origin-cell march self-shadow
	/// without darkening every diagonal.
	#[test]
	fn a_ramp_plane_lies_on_the_air_side_of_its_mask() {
		use crate::shapes::{shape_of, state_valid};
		let mut ramps = 0;
		for b in BlockType::ALL {
			if b.props().shape.is_none() {
				continue;
			}
			for state in 0..16u8 {
				let cell = make_cell(b, state);
				if !state_valid(cell) || gi_cover(cell) == 0 {
					continue;
				}
				let c = &covers()[usize::from(gi_cover(cell)) - 1];
				let v = shape_of(b, state).unwrap();
				for q in v.quads.iter().filter(|q| q.is_ramp()) {
					let (ox, oy, oz) = q.dir.offset();
					let mut ctr = [0f32; 3];
					for corner in q.corners {
						for a in 0..3 {
							ctr[a] += f32::from(corner[a]) * 0.25;
						}
					}
					let at = [
						ctr[0] + ox as f32 * 0.25,
						ctr[1] + oy as f32 * 0.25,
						ctr[2] + oz as f32 * 0.25,
					]
					.map(|v| v.floor());
					if at.iter().any(|&v| !(0.0..12.0).contains(&v)) {
						continue;
					}
					let [x, y, z] = at.map(|v| v as u8);
					assert!(
						!c.filled(x, y, z),
						"{b:?} state {state}: ramp {q:?} starts in its mask"
					);
					ramps += 1;
				}
			}
		}
		assert!(ramps > 0, "the lumber stairs ship ramps");
	}

	#[test]
	fn a_preview_cell_classifies_to_its_rewritable_slot() {
		use crate::shapes::{clear_preview_overrides, preview_variant, set_preview_override};
		let chair = make_cell(BlockType::Chair, 2);
		let own = gi_cover(chair);
		assert_ne!(own, 0);
		set_preview_override(
			BlockType::Chair,
			2,
			preview_variant(&[], &[], [1, 1, 1], (0, 0, 0), 0),
		);
		assert_eq!(gi_cover(chair), preview_cover_slot(2));
		assert_eq!(gi_look(chair), gi_look(BlockType::Chair as u16));
		// Another state of the same block is untouched.
		assert_eq!(
			gi_cover(make_cell(BlockType::Chair, 1)),
			gi_cover(make_cell(BlockType::Chair, 1))
		);
		clear_preview_overrides();
		assert_eq!(gi_cover(chair), own);
		// The slots sit above every shipped cover, one per footprint part.
		for part in 0..PREVIEW_COVER_SLOTS as u8 {
			let slot = usize::from(preview_cover_slot(part));
			assert!(slot > covers().len());
			assert!(slot < PALETTE);
		}
	}

	#[test]
	fn box_pack_matches_the_shader_decode() {
		// gi_box_span unpacks the three min coordinates and then the three
		// max, a nibble each. 0 must remain the no-box sentinel.
		let b = SubBox::new([0, 8, 3], [12, 12, 7]);
		let p = pack_box(b);
		assert_ne!(p, 0);
		let dec = |v: u32, s: u32| (v >> s) & 0xF;
		assert_eq!([dec(p, 0), dec(p, 4), dec(p, 8)], [0, 8, 3], "min lanes");
		assert_eq!(
			[dec(p, 12), dec(p, 16), dec(p, 20)],
			[12, 12, 7],
			"max lanes"
		);
		// Every real cover's bounds pack nonzero (max is never all-zero).
		for c in covers() {
			assert_ne!(pack_box(c.bounds), 0);
		}
	}

	#[test]
	fn air_chunk_classifies_to_zeros() {
		let gi = classify_chunk(&ChunkData::default());
		assert!(gi.bytes.iter().all(|&b| b == 0));
		assert_eq!(gi.coarse, [0; 8]);
		assert!(gi.emitters.is_empty());
	}

	#[test]
	fn bytes_are_texel_ordered_not_chunk_ordered() {
		// One stone cell off every axis: its texel must sit at the
		// write_texture layout index (z-major, then y, x fastest), which
		// differs from ChunkData's y-major order — look first, cover
		// beside it.
		let mut data = ChunkData::default();
		data.set(3, 5, 7, BlockType::Stone);
		let gi = classify_chunk(&data);
		let i = ((7 * 16 + 5) * 16 + 3) * BYTES_PER_CELL;
		assert_eq!(gi.bytes[i], gi_look(BlockType::Stone as u16));
		assert_eq!(gi.bytes[i + 1], 0);
		assert_eq!(gi.bytes.iter().filter(|&&b| b != 0).count(), 1);
		// And a shaped cell writes both bytes.
		let mut data = ChunkData::default();
		data.set(3, 5, 7, BlockType::WoodSlab);
		let gi = classify_chunk(&data);
		assert_eq!(gi.bytes[i], gi_look(BlockType::Wood as u16));
		assert_eq!(gi.bytes[i + 1], gi_cover(make_cell(BlockType::WoodSlab, 0)));
	}

	#[test]
	fn coarse_bits_split_blockers_from_emitters() {
		let mut data = ChunkData::default();
		// Stone in the lowest coarse cell, a torch in the far corner one.
		data.set(2, 2, 2, BlockType::Stone);
		data.set(12, 12, 12, BlockType::Torch);
		let gi = classify_chunk(&data);
		assert_eq!(gi.coarse[0] & 0b11, 0b01, "stone: blocks, no emission");
		// The torch blocks (partial cells still pin their brick) and
		// emits, carrying the post's cover.
		assert_eq!(gi.coarse[7] & 0b11, 0b11);
		let (look, cover) = gi_classes(BlockType::Torch as u16);
		assert_ne!(cover, 0);
		assert_eq!(
			gi.emitters,
			vec![GiEmitter {
				at: [12, 12, 12],
				look,
				cover
			}]
		);
		// Water attenuates but neither blocks nor emits.
		let mut wet = ChunkData::default();
		wet.set(0, 0, 0, BlockType::Water);
		let w = classify_chunk(&wet);
		assert_eq!(w.coarse, [0; 8]);
		assert_eq!(opacity(BlockType::Water), GiOpacity::Translucent);
		assert_ne!(w.bytes[0], 0, "water still classifies — rays attenuate");
		// A tinted cell PINS its brick — a skipped brick accumulates no
		// throughput, so a wall of panes would go untinted — but it
		// still emits nothing.
		let mut pane = ChunkData::default();
		pane.set(0, 0, 0, BlockType::Glass);
		let g = classify_chunk(&pane);
		assert_eq!(
			g.coarse[0] & 0b11,
			0b01,
			"glass: pins the brick, no emission"
		);
		assert_eq!(opacity(BlockType::Glass), GiOpacity::Tinted);
	}

	/// The far gather crosses a brick by its summary, so a brick with any blocker must be
	/// unskippable AND carry a top, the mode must be the look most blockers wear, a solid brick
	/// must read as a full core, and the word must round-trip the summary it was packed from.
	#[test]
	fn a_brick_with_a_blocker_is_never_open() {
		let mut data = ChunkData::default();
		let mut examined = 0;
		let mut b = Brick::default();
		for n in 0..BRICK_CELLS {
			let (x, y, z) = (n % BRICK, (n / BRICK) % BRICK, n / (BRICK * BRICK));
			data.set(
				x,
				y,
				z,
				if n % 3 == 2 {
					BlockType::Dirt
				} else {
					BlockType::Stone
				},
			);
			let w = classify_chunk(&data).coarse[0];
			b = unpack_coarse(w);
			assert!(
				b.hi > 0 && (w & 1) == 1,
				"a brick with a blocker is never skippable"
			);
			assert!(b.lo <= b.hi);
			assert!(
				b.core > 0 || b.band > 0,
				"some layer of the brick must be a medium"
			);
			assert_eq!(
				b.modal,
				gi_look(BlockType::Stone as u16),
				"the mode is the look most blockers wear"
			);
			assert_eq!(
				unpack_coarse(pack_coarse(true, false, b)),
				b,
				"the word round-trips"
			);
			examined += 1;
		}
		assert_eq!(examined, BRICK_CELLS);
		assert_eq!(
			(b.lo, b.hi, b.core),
			(BRICK as u8, BRICK as u8, CORE_FULL),
			"a solid brick is all core"
		);
		assert_eq!(classify_chunk(&ChunkData::default()).coarse[0], 0);
		// Water attenuates without blocking, so it counts toward nothing.
		let mut wet = ChunkData::default();
		wet.set(1, 1, 1, BlockType::Water);
		assert_eq!(classify_chunk(&wet).coarse[0], 0);
	}

	/// The heightfield half of the summary: flat ground inside a brick is ALL core and no band
	/// (so a ray skimming it is open), and one pillar on that ground opens a band exactly as
	/// tall as the pillar, at the pillar's own thin density, leaving the core untouched.
	#[test]
	fn the_brick_summary_is_a_heightfield() {
		let floor = 3;
		let mut data = ChunkData::default();
		for z in 0..BRICK {
			for x in 0..BRICK {
				for y in 0..floor {
					data.set(x, y, z, BlockType::Stone);
				}
			}
		}
		let flat = unpack_coarse(classify_chunk(&data).coarse[0]);
		assert_eq!((flat.lo, flat.hi), (floor as u8, floor as u8));
		assert_eq!((flat.core, flat.band), (CORE_FULL, 0));
		let pillar = 4;
		for y in floor..floor + pillar {
			data.set(5, y, 5, BlockType::Stone);
		}
		let b = unpack_coarse(classify_chunk(&data).coarse[0]);
		assert_eq!((b.lo, b.hi), (floor as u8, (floor + pillar) as u8));
		assert_eq!(b.core, CORE_FULL, "the floor is still the core");
		let band_cells = pillar * BRICK * BRICK;
		assert_eq!(b.band, density(pillar, band_cells, BAND_FULL));
		assert!(
			b.band > 0 && b.band < BAND_FULL,
			"a lone pillar is a thin band, never a wall"
		);
	}
}
