//! GI-class classification for the RTGI voxel window (GI-DESIGN.md).
//!
//! The window is an R8Uint 3D texture, one texel per block, that the
//! probe rays march. Classification is a pure function of the cell so
//! every consumer (arrival uploads, edit re-uploads, tests) agrees by
//! construction. The class byte IS the block id (state nibble dropped;
//! ids fit u8 with room to spare — pinned below): everything a ray
//! needs per class — opacity, albedo, emission — lives in the render
//! side's 256-entry material LUT, so the window never re-uploads when
//! a material's look is tuned.
//!
//! v1 opacity model, deliberately coarse (the same spirit as "fluid
//! treats shapes as solid"): RenderClass::None = air, Translucent =
//! water (rays attenuate), everything else — Opaque, Cutout, Shaped —
//! blocks rays whole. A canopy therefore shades and a torch post stops
//! a ray, which is acceptable at probe resolution.

use crate::block::{BlockType, RenderClass};
use crate::chunk::{ChunkData, CHUNK_SIZE, CHUNK_VOLUME};

/// What a ray does when it enters a cell of this class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GiOpacity {
	Air,
	Translucent,
	Opaque,
}

/// One classified chunk, ready for the GPU: texel bytes in
/// write_texture order, the /8 coarse occupancy bytes, and the cells
/// that emit (for the light list).
pub struct GiChunk {
	/// `[z][y][x]` — depth slices of `bytes_per_row = 16`,
	/// `rows_per_image = 16` (NOT ChunkData's y-major order).
	pub bytes: Box<[u8; CHUNK_VOLUME]>,
	/// 2×2×2 coarse cells of 8³ blocks each, `[z][y][x]`;
	/// bit 0 = any ray-blocking cell, bit 1 = any emitting cell.
	pub coarse: [u8; 8],
	/// Emitting cells as (local x, y, z, class).
	pub emitters: Vec<(u8, u8, u8, u8)>,
}

/// The class byte for one cell: 0 for air, else the block id. The
/// state nibble is dropped — shaped variants share their base block's
/// look at ray resolution.
#[inline]
pub fn gi_class(cell: u16) -> u8 {
	let b = BlockType::from_id(crate::chunk::cell_id(cell));
	if b == BlockType::Air {
		0
	} else {
		b as u8
	}
}

/// How a class attenuates rays (LUT-side mirror; also the coarse rule).
#[inline]
pub fn opacity(b: BlockType) -> GiOpacity {
	match b.props().class {
		RenderClass::None => GiOpacity::Air,
		RenderClass::Translucent => GiOpacity::Translucent,
		_ => GiOpacity::Opaque,
	}
}

/// Classify a whole chunk. Air chunks classify to all-zero bytes —
/// callers still upload them, because the window is toroidal and the
/// region may hold a stale previous tenant.
pub fn classify_chunk(data: &ChunkData) -> GiChunk {
	let n = CHUNK_SIZE;
	let mut bytes = Box::new([0u8; CHUNK_VOLUME]);
	let mut coarse = [0u8; 8];
	let mut emitters = Vec::new();
	for z in 0..n {
		for y in 0..n {
			for x in 0..n {
				let cell = data.get_cell(x, y, z);
				let class = gi_class(cell);
				bytes[(z * n + y) * n + x] = class;
				if class == 0 {
					continue;
				}
				let b = BlockType::from_id(crate::chunk::cell_id(cell));
				let emits = b.props().light > 0;
				if emits {
					emitters.push((x as u8, y as u8, z as u8, class));
				}
				let blocks_rays = opacity(b) == GiOpacity::Opaque;
				if blocks_rays || emits {
					let ci = (z / 8) * 4 + (y / 8) * 2 + (x / 8);
					coarse[ci] |= u8::from(blocks_rays) | (u8::from(emits) << 1);
				}
			}
		}
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

	#[test]
	fn every_block_id_fits_the_class_byte() {
		// The class byte is the id; the LUT has 256 entries. A block
		// catalog crossing 255 needs a wider window format first.
		#[allow(clippy::assertions_on_constants)]
		{
			assert!(BlockType::COUNT <= 256);
		}
		for b in BlockType::ALL {
			assert_eq!(
				gi_class(b as u16),
				if b == BlockType::Air { 0 } else { b as u8 }
			);
		}
	}

	#[test]
	fn class_ignores_the_state_nibble() {
		let cell = (BlockType::StoneStairs as u16) | (0b0101 << 12);
		assert_eq!(gi_class(cell), BlockType::StoneStairs as u8);
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
		// One stone cell at (x=3, y=5, z=7): the byte must sit at the
		// write_texture layout index (z*16 + y)*16 + x, which differs
		// from ChunkData's (y*16 + z)*16 + x.
		let mut data = ChunkData::default();
		data.set(3, 5, 7, BlockType::Stone);
		let gi = classify_chunk(&data);
		assert_eq!(gi.bytes[(7 * 16 + 5) * 16 + 3], BlockType::Stone as u8);
		assert_eq!(gi.bytes.iter().filter(|&&b| b != 0).count(), 1);
	}

	#[test]
	fn coarse_bits_split_blockers_from_emitters() {
		let mut data = ChunkData::default();
		// Stone in the low 8³ region, a torch in the high corner region.
		data.set(2, 2, 2, BlockType::Stone);
		data.set(12, 12, 12, BlockType::Torch);
		let gi = classify_chunk(&data);
		assert_eq!(gi.coarse[0], 0b01, "stone: blocks, no emission");
		// The torch both blocks (Shaped → opaque v1) and emits.
		assert_eq!(gi.coarse[7], 0b11);
		assert_eq!(gi.emitters, vec![(12, 12, 12, BlockType::Torch as u8)]);
		// Water attenuates but neither blocks nor emits.
		let mut wet = ChunkData::default();
		wet.set(0, 0, 0, BlockType::Water);
		let w = classify_chunk(&wet);
		assert_eq!(w.coarse, [0; 8]);
		assert_eq!(opacity(BlockType::Water), GiOpacity::Translucent);
		assert_ne!(w.bytes[0], 0, "water still classifies — rays attenuate");
	}
}
