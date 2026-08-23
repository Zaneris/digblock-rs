//! Surface emitters: turning a revealed vein into area lights (R31).
//!
//! A torch is a point in open air. A vein is not: most of it is buried, buried rock must light
//! nothing, and the exposed part is a FACE — an area source.
//!
//! **Why this lives in the mesher**: it is the only pass holding all three inputs at once — the
//! reveal has already run, so the padded array holds real ore ids for exactly the tiers this
//! player has levelled into; exposure is the question the mesher exists to answer; and the padded
//! shell means a vein straddling a chunk edge does not go dark down one column.
//! `crate::gi::classify_chunk` can do none of it, because it reads the raw store and the store
//! holds Stone where a vein is.
//!
//! **Merging is not only an optimisation.** The resolve's NEE reservoir walks the whole light
//! list *per pixel*, and the merged rectangle is also the better light: one quad with a cosine at
//! its own surface, rather than a row of points each with its own hotspot.

use crate::block::BlockType;
use crate::chunk::CHUNK_SIZE;
use crate::dir::{DIRS, Dir};

use super::{MeshInput, face_visible, pidx};

/// One emissive rectangle, packed into a word — the same shape [`super::MeshData::impacts`] uses
/// for spray, encoded on the wire by the same run. Fields in order: the anchor cell (chunk-local,
/// a nibble an axis), the face direction, the two spans along [`tangents`] (stored one less than
/// the span, so a span always fits its nibble), and the block id whose LUT row carries the
/// light's level and colour. Pinned by `a_packed_rect_round_trips`; no offset escapes the decoder.
const X_SHIFT: u32 = 0;
const Y_SHIFT: u32 = 4;
const Z_SHIFT: u32 = 8;
const DIR_SHIFT: u32 = 12;
const SPAN_U_SHIFT: u32 = 15;
const SPAN_V_SHIFT: u32 = 19;
const ID_SHIFT: u32 = 23;
const NIBBLE: u32 = 0xF;

/// The two axes a face of `dir` spans, in ascending axis order, so a rectangle's `u` and `v` mean
/// the same thing to the packer and to the renderer that unpacks it. `0`/`1`/`2` are x/y/z.
/// Ascending rather than a handedness convention: the rectangle is axis-aligned and its two spans
/// are interchangeable, so there is nothing for a winding order to buy and one less thing for the
/// two ends to disagree about.
#[inline]
pub const fn tangents(dir: Dir) -> (usize, usize) {
	match dir {
		Dir::West | Dir::East => (1, 2),
		Dir::Bottom | Dir::Top => (0, 2),
		Dir::North | Dir::South => (0, 1),
	}
}

/// Pack one rectangle. `span_u`/`span_v` are counts of cells, at least 1.
#[inline]
pub fn pack(cell: (usize, usize, usize), dir: Dir, span_u: u32, span_v: u32, id: u16) -> u32 {
	debug_assert!((1..=CHUNK_SIZE as u32).contains(&span_u));
	debug_assert!((1..=CHUNK_SIZE as u32).contains(&span_v));
	(cell.0 as u32) << X_SHIFT
		| (cell.1 as u32) << Y_SHIFT
		| (cell.2 as u32) << Z_SHIFT
		| (dir as u32) << DIR_SHIFT
		| (span_u - 1) << SPAN_U_SHIFT
		| (span_v - 1) << SPAN_V_SHIFT
		| u32::from(id) << ID_SHIFT
}

/// Unpack one rectangle: anchor cell, direction, the two spans in cells,
/// and the block id. The renderer's half of [`pack`].
#[inline]
pub fn unpack(w: u32) -> ((u32, u32, u32), Dir, u32, u32, u16) {
	let dir = DIRS[((w >> DIR_SHIFT) & 0x7) as usize];
	(
		(
			(w >> X_SHIFT) & NIBBLE,
			(w >> Y_SHIFT) & NIBBLE,
			(w >> Z_SHIFT) & NIBBLE,
		),
		dir,
		((w >> SPAN_U_SHIFT) & NIBBLE) + 1,
		((w >> SPAN_V_SHIFT) & NIBBLE) + 1,
		((w >> ID_SHIFT) & 0xFF) as u16,
	)
}

/// Every exposed face of every surface emitter in the chunk, greedy-merged into rectangles. Its
/// own sweep rather than part of the face loop because merging needs a whole slice at once, and
/// because the vast majority of chunks hold no emitter at all and should pay one scan and leave.
pub fn collect(input: &MeshInput) -> Vec<u32> {
	let n = CHUNK_SIZE;
	// Cheap reject: no surface emitter anywhere means no slices to build.
	// Most chunks in a world have never seen a vein.
	if !(0..n).any(|y| {
		(0..n).any(|z| (0..n).any(|x| is_surface_emitter(input.get(x as i32, y as i32, z as i32))))
	}) {
		return Vec::new();
	}
	let mut out = Vec::new();
	// Per direction, per slice: a grid of "which block id lights this face,
	// or none". Merging is per id — two tiers meeting in one pocket are two
	// colours and two levels, and a rectangle is one of each.
	let mut mask = vec![0u16; n * n];
	for &dir in &DIRS {
		let (au, av) = tangents(dir);
		// The axis the slices stack along is the one neither tangent is.
		let an = 3 - au - av;
		for slice in 0..n {
			mask.fill(0);
			let mut any = false;
			for v in 0..n {
				for u in 0..n {
					let mut cell = [0usize; 3];
					cell[an] = slice;
					cell[au] = u;
					cell[av] = v;
					let (x, y, z) = (cell[0], cell[1], cell[2]);
					let id = input.get(x as i32, y as i32, z as i32);
					if !is_surface_emitter(id) {
						continue;
					}
					// Exposed by exactly the mesher's own rule — the same
					// `face_visible`, the same missing-neighbour suppression — so a lit
					// face and a DRAWN face are the same set by construction. A seam face
					// whose neighbour has not arrived stays dark until it does.
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
						let pi = pidx(x as i32, y as i32, z as i32);
						Some(input.blocks[(pi as isize + super::PDELTA[dir as usize]) as usize])
					};
					if !face_visible(BlockType::from_id(id), neighbor, dir) {
						continue;
					}
					mask[v * n + u] = id;
					any = true;
				}
			}
			if any {
				merge_slice(&mut mask, n, slice, dir, an, au, av, &mut out);
			}
		}
	}
	out
}

/// Greedy-merge one slice's mask into maximal rectangles of one id: take the first unclaimed
/// cell, run as far as it can along `u`, then grow along `v` while every cell of the next row
/// matches, clearing as it goes. Maximal in `u` first, which is arbitrary and fine — they are
/// lights, not geometry, and no seam can show between two of them.
#[allow(clippy::too_many_arguments)]
fn merge_slice(
	mask: &mut [u16],
	n: usize,
	slice: usize,
	dir: Dir,
	an: usize,
	au: usize,
	av: usize,
	out: &mut Vec<u32>,
) {
	for v in 0..n {
		let mut u = 0;
		while u < n {
			let id = mask[v * n + u];
			if id == 0 {
				u += 1;
				continue;
			}
			let mut w = 1;
			while u + w < n && mask[v * n + u + w] == id {
				w += 1;
			}
			let mut h = 1;
			while v + h < n && (0..w).all(|k| mask[(v + h) * n + u + k] == id) {
				h += 1;
			}
			for dv in 0..h {
				for du in 0..w {
					mask[(v + dv) * n + u + du] = 0;
				}
			}
			let mut cell = [0usize; 3];
			cell[an] = slice;
			cell[au] = u;
			cell[av] = v;
			out.push(pack(
				(cell[0], cell[1], cell[2]),
				dir,
				w as u32,
				h as u32,
				id,
			));
			u += w;
		}
	}
}

#[inline]
fn is_surface_emitter(id: u16) -> bool {
	let p = BlockType::from_id(id).props();
	p.surface_light && p.light > 0
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block::BlockType as B;
	use crate::chunk::ChunkData;

	fn input_of(d: &ChunkData) -> MeshInput {
		MeshInput::build(d, |_, _, _| crate::mesher::Src::Missing)
	}

	#[test]
	fn a_packed_rect_round_trips() {
		for dir in DIRS {
			let w = pack((3, 15, 0), dir, 16, 1, B::Aetherock as u16);
			let (cell, d, su, sv, id) = unpack(w);
			assert_eq!(cell, (3, 15, 0));
			assert_eq!(d, dir);
			assert_eq!((su, sv), (16, 1));
			assert_eq!(id, B::Aetherock as u16);
		}
		// The widest anything can be, in every field at once — the packing
		// has to survive its own corners, and the block id is the field
		// with room to spare rather than the one to watch.
		let w = pack((15, 15, 15), Dir::East, 16, 16, B::MastersSigil as u16);
		let (cell, d, su, sv, id) = unpack(w);
		assert_eq!(
			(cell, d, su, sv, id),
			((15, 15, 15), Dir::East, 16, 16, B::MastersSigil as u16)
		);
	}

	/// A lone vein cell in open air lights all six ways; the same cell
	/// walled in lights nothing at all. The second half is the point of the
	/// whole module — buried rock has no face to radiate from, and a point
	/// source in there would leak through its own wall.
	#[test]
	fn only_exposed_faces_emit() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Glowvein);
		let free = collect(&input_of(&d));
		assert_eq!(free.len(), 6, "one rectangle per open face");
		for &w in &free {
			let (cell, _, su, sv, id) = unpack(w);
			assert_eq!(cell, (8, 8, 8));
			assert_eq!((su, sv), (1, 1));
			assert_eq!(id, B::Glowvein as u16);
		}
		for (dx, dy, dz) in [
			(1, 0, 0),
			(-1, 0, 0),
			(0, 1, 0),
			(0, -1, 0),
			(0, 0, 1),
			(0, 0, -1),
		] {
			d.set(
				(8 + dx) as usize,
				(8 + dy) as usize,
				(8 + dz) as usize,
				B::Stone,
			);
		}
		assert!(
			collect(&input_of(&d)).is_empty(),
			"a buried vein lights nothing"
		);
	}

	/// A flat seam in a wall is ONE light, not one per cell. This is the
	/// property the resolve's per-pixel light loop cares about.
	#[test]
	fn a_flat_seam_merges_into_one_rectangle() {
		let mut d = ChunkData::default();
		// A 3x2 patch of vein in the y = 8 plane, capped from below so only
		// the top faces are open.
		for z in 4..6 {
			for x in 4..7 {
				d.set(x, 8, z, B::Emberore);
				d.set(x, 7, z, B::Stone);
			}
		}
		let rects = collect(&input_of(&d));
		let tops: Vec<_> = rects
			.iter()
			.map(|&w| unpack(w))
			.filter(|&(_, dir, ..)| dir == Dir::Top)
			.collect();
		assert_eq!(tops.len(), 1, "six open top faces, one light: {tops:?}");
		let (cell, _, su, sv, id) = tops[0];
		assert_eq!(cell, (4, 8, 4));
		assert_eq!((su, sv), (3, 2), "spans x then z");
		assert_eq!(id, B::Emberore as u16);
	}

	/// Two tiers touching never merge: a rectangle carries ONE block id,
	/// and the id is where the light's colour and level come from.
	#[test]
	fn different_tiers_never_share_a_rectangle() {
		let mut d = ChunkData::default();
		d.set(4, 8, 4, B::Emberore);
		d.set(5, 8, 4, B::Aetherock);
		d.set(4, 7, 4, B::Stone);
		d.set(5, 7, 4, B::Stone);
		let tops: Vec<_> = collect(&input_of(&d))
			.iter()
			.map(|&w| unpack(w))
			.filter(|&(_, dir, ..)| dir == Dir::Top)
			.collect();
		assert_eq!(tops.len(), 2, "{tops:?}");
		assert!(tops.iter().all(|&(_, _, su, sv, _)| (su, sv) == (1, 1)));
	}

	/// A block that merely `glows` is not a surface emitter: ink is what
	/// you SEE, `seams` is what it throws, and Crystal deliberately has
	/// only the first.
	#[test]
	fn ink_alone_is_not_a_light() {
		let mut d = ChunkData::default();
		d.set(8, 8, 8, B::Crystal);
		assert!(B::Crystal.props().emissive, "fixture needs the ink");
		assert!(collect(&input_of(&d)).is_empty());
		// Nor is a torch, which is a POINT source and belongs to
		// `gi::classify_chunk` — counting it here would double it.
		let mut t = ChunkData::default();
		t.set(8, 8, 8, B::Torch);
		assert!(B::Torch.props().light > 0, "fixture needs the cast");
		assert!(collect(&input_of(&t)).is_empty());
	}
}
