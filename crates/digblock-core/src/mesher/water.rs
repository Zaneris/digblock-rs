//! The water surface system: shared corner heights, fall lips, flow
//! and splash codes, and the two-word water vertex emission. Split out
//! of the solid mesher so the hot solid loop and the (deliberately
//! outlined) water path evolve independently.

use super::{face_corners, occludes, pack, Dir, MeshInput, Vert};
use crate::block::{BlockType, RenderClass};

/// The amount a water cell contributes to shared surface-corner
/// heights. Falling cells don't surface at their nominal (full)
/// amount: the sheet's lip continues the feeder's downward slope, one
/// level below the highest adjacent resting water — a source (8) feeds
/// a 7-high lip, a thin flow a thinner one — instead of popping back
/// up to full height at the edge. No feeder in reach (e.g. a floating
/// column) falls back to the nominal amount.
fn surface_amount(input: &MeshInput, x: i32, y: i32, z: i32, b: BlockType) -> Option<u8> {
	let a = b.water_amount()?;
	if !b.is_falling() {
		return Some(a);
	}
	let mut feeder = 0u8;
	for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
		let (nx, nz) = (x + dx, z + dz);
		// Corner cells sit at -1..=16; their neighbors can leave the
		// padded snapshot — skip those.
		if !(-1..=16).contains(&nx) || !(-1..=16).contains(&nz) {
			continue;
		}
		let nb = BlockType::from_id(input.get(nx, y, nz));
		if nb.is_water() && !nb.is_falling() {
			feeder = feeder.max(nb.water_amount().unwrap_or(0));
		}
	}
	if feeder > 0 {
		Some((feeder - 1).max(1))
	} else {
		Some(a)
	}
}

/// Water surface drop code at horizontal lattice corner (lx, lz) for
/// water cells at height y: the corner touches up to four cells.
/// Priority: if any touching column has water at y + 1 the surface is
/// pinned full (d = 0) — a submerged neighbor, or water hanging over a
/// step this corner must rise to meet. Else, if any touching cell is
/// open air with water directly below, the surface continues a step
/// down and this corner plunges the whole block (d = 9), so cascades
/// render as connected ramps instead of stacked cliff slivers. Else
/// the drop is 9 − round(mean water amount). Adjacent cells sharing
/// the corner agree, so surfaces knit without cracks — including
/// across chunk borders via the padded shell.
fn corner_drop(input: &MeshInput, lx: i32, y: i32, lz: i32) -> u32 {
	let (mut sum, mut count) = (0u32, 0u32);
	let mut plunge = false;
	for cz in lz - 1..=lz {
		for cx in lx - 1..=lx {
			// Cell y is 0..=15, so y ± 1 stays inside the padded range.
			if BlockType::from_id(input.get(cx, y + 1, cz)).is_water() {
				return 0;
			}
			let b = BlockType::from_id(input.get(cx, y, cz));
			match surface_amount(input, cx, y, cz, b) {
				Some(a) => {
					sum += u32::from(a);
					count += 1;
				}
				None if b.props().class == RenderClass::None => {
					plunge |= BlockType::from_id(input.get(cx, y - 1, cz)).is_water();
				}
				None => {}
			}
		}
	}
	if plunge {
		return 9;
	}
	debug_assert!(count > 0, "corner of a water face touches its own cell");
	let a = (2 * sum + count) / (2 * count); // round half up
	9 - a
}

/// Per-face water surface state, computed once per cell in `mesh_chunk`.
#[derive(Clone, Copy)]
pub(super) struct WaterState {
	/// Water directly above: render a complete cube. The top block of a
	/// fall column has air above and takes shared corner heights like
	/// any surface cell, so the lip knits with the pool feeding it.
	pub(super) full: bool,
	pub(super) falling: bool,
	/// Horizontal flow direction code (0 still, 1 +x, 2 −x, 3 +z, 4 −z):
	/// the side with the least water, where in-transit water is headed.
	pub(super) flow: u32,
	/// Splash code: 3 = a fall's impact cell (falling water striking
	/// the pool surface or solid ground below), 2/1 = surface-water
	/// cells at Chebyshev distance 1/2 from an impact — the shader's
	/// expanding foam rings. 0 elsewhere.
	pub(super) splash: u32,
	/// Contiguous falling cells above this one (0 = sheet top), capped
	/// at 15. When the walk hits the snapshot ceiling with the column
	/// still falling, this saturates to 15 rather than reporting the
	/// partial count: a fall crossing a vertical chunk seam whitens a
	/// little early (masked by the splash below), where the partial
	/// count would visibly reset the sheet to a glassy lip mid-fall.
	/// Exact depth would need depth-encoded block variants.
	pub(super) fall_dist: u32,
}

impl WaterState {
	/// Outlined: inlining this (and emit_water_face) into mesh_chunk's
	/// block loop measurably slowed the dominant solid path (~+0.03ms on
	/// a 0.08ms avg mesh job) — water cells are the rare case.
	#[inline(never)]
	pub(super) fn of(input: &MeshInput, x: i32, y: i32, z: i32, me: BlockType) -> WaterState {
		let falling = me.is_falling();
		let above = BlockType::from_id(input.get(x, y + 1, z));
		let full = above.is_water();
		let flow = if falling || me.is_fluid_source() {
			0 // sources rest, falls scroll vertically instead
		} else {
			let amount = |dx: i32, dz: i32| -> u8 {
				BlockType::from_id(input.get(x + dx, y, z + dz))
					.water_amount()
					.unwrap_or(0)
			};
			let sides = [(1, 0), (-1, 0), (0, 1), (0, -1)];
			let amounts = sides.map(|(dx, dz)| amount(dx, dz));
			let min = *amounts.iter().min().expect("four sides");
			if amounts.iter().all(|&a| a == min) {
				0
			} else {
				1 + amounts.iter().position(|&a| a == min).expect("has min") as u32
			}
		};
		// A fall's landing: falling water whose below-cell is where the
		// fall ends — resting water (worldgen falls land in pools) or
		// solid ground (fluid-sim falls punching to a floor).
		let is_impact = |nx: i32, ny: i32, nz: i32| -> bool {
			if !BlockType::from_id(input.get(nx, ny, nz)).is_falling() {
				return false;
			}
			let below_raw = input.get(nx, ny - 1, nz);
			let below = BlockType::from_id(below_raw);
			!below.is_falling() && (below.is_water() || occludes(below_raw))
		};
		let splash = if falling && is_impact(x, y, z) {
			3
		} else if !falling && !full {
			// Ring scan: nearest impact in the 5×5 neighborhood at this
			// level or one above (worldgen falls land one cell above
			// the pool surface). Chebyshev 1 → 2, 2 → 1. An impact two
			// blocks into a neighbor chunk sits outside the padded
			// snapshot — a one-cell notch in an animated ring.
			let mut best = 0u32;
			for dz in -2i32..=2 {
				for dx in -2i32..=2 {
					if dx == 0 && dz == 0 {
						continue;
					}
					let (nx, nz) = (x + dx, z + dz);
					if !(-1..=16).contains(&nx) || !(-1..=16).contains(&nz) {
						continue;
					}
					let code = 3 - dx.abs().max(dz.abs()) as u32;
					if code > best && (is_impact(nx, y, nz) || is_impact(nx, y + 1, nz)) {
						best = code;
					}
				}
			}
			best
		} else {
			0
		};
		let fall_dist = if falling {
			let mut d = 0u32;
			let mut cy = y + 1;
			while cy <= 16 && BlockType::from_id(input.get(x, cy, z)).is_falling() {
				d += 1;
				cy += 1;
			}
			if cy > 16 {
				15
			} else {
				d.min(15)
			}
		} else {
			0
		};
		WaterState {
			full,
			falling,
			flow,
			splash,
			fall_dist,
		}
	}

	/// Word-1 bits above the per-vertex drop code.
	fn flags(self) -> u32 {
		u32::from(self.falling) << 4 | self.flow << 5 | self.splash << 8
	}
}

/// Emit one water face quad: two words per vertex. A `full` cell
/// (water directly above) renders as a complete cube; otherwise
/// vertices on the cell's top lattice plane drop to their corner's
/// shared surface height. Outlined (like `WaterState::of`) so the
/// rare water path doesn't bloat mesh_chunk's hot solid loop.
#[inline(never)]
pub(super) fn emit_water_face(
	out: &mut Vec<u32>,
	input: &MeshInput,
	b: (usize, usize, usize),
	dir: Dir,
	tex: u32,
	state: WaterState,
) {
	let (bx, by, bz) = (b.0 as i32, b.1 as i32, b.2 as i32);
	let corners = face_corners(bx, by, bz, dir);
	let mut drops = [0u32; 4];
	for (ci, &(cx, cy, cz)) in corners.iter().enumerate() {
		if !state.full && cy == by + 1 {
			drops[ci] = corner_drop(input, cx, by, cz);
		}
	}
	// A side face whose whole top edge plunged the full block (d = 9)
	// collapsed onto its bottom edge — zero area, skip it.
	if dir != Dir::Top && dir != Dir::Bottom {
		let top_gone = corners
			.iter()
			.zip(&drops)
			.all(|(&(_, cy, _), &d)| cy != by + 1 || d == 9);
		if top_gone {
			return;
		}
	}
	for (ci, &(cx, cy, cz)) in corners.iter().enumerate() {
		let d = drops[ci];
		// Per-vertex fall distance: top-plane verts carry the cell's
		// count, bottom-plane verts one more, so interpolation yields a
		// continuous "blocks fallen" down stacked cells.
		let vfall = if state.falling {
			(state.fall_dist + u32::from(cy == by)).min(15)
		} else {
			0
		};
		out.push(pack(
			Vert {
				pos: (cx as u32, cy as u32, cz as u32),
				tex,
				ao: 3,
				tint: 0,
			},
			dir,
			ci as u32,
		));
		out.push(d | state.flags() | (vfall << 10));
	}
}
