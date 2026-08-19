//! The water surface system: shared corner heights, fall lips, flow
//! and splash codes, and the two-word water vertex emission. Split out
//! of the solid mesher so the hot solid loop and the (deliberately
//! outlined) water path evolve independently.

use super::{Dir, MeshInput, PADDED, face_corners, occludes, pack};
use crate::block::{BlockType, RenderClass};
use crate::chunk::ID_MASK;

/// Which padded y layers hold falling water, swept lazily one layer at a
/// time. The splash ring scan below walks a square neighbourhood across
/// two layers per open surface cell — dozens of scattered padded reads
/// each, and a full ocean surface chunk pays that once per column — yet it
/// can only ever score off a FALLING cell, and almost no chunk has one. A
/// layer sweep is one contiguous plane, at most once per layer per mesh
/// job, and an ocean surface touches two layers.
#[derive(Default)]
pub(super) struct FallLayers {
	/// One bit per padded layer, biased by the shell so the layer below
	/// the chunk still has a bit of its own.
	swept: u32,
	any: u32,
}

impl FallLayers {
	/// Can the ring scan at cell layer `y` find anything? It reads layers
	/// y and y + 1 only, so falling water outside that pair is invisible
	/// to it — skipping on false is exact, never a heuristic.
	fn reachable(&mut self, input: &MeshInput, y: i32) -> bool {
		// Bitwise, not short-circuiting: both layers get swept either way
		// and the neighbouring cell asks again immediately.
		self.layer(input, y) | self.layer(input, y + 1)
	}

	fn layer(&mut self, input: &MeshInput, y: i32) -> bool {
		let bit = 1u32 << (y + 1) as u32;
		if self.swept & bit == 0 {
			self.swept |= bit;
			let base = (y + 1) as usize * PADDED * PADDED;
			let plane = &input.blocks[base..base + PADDED * PADDED];
			if plane
				.iter()
				.any(|&c| BlockType::from_id(c & ID_MASK).is_falling())
			{
				self.any |= bit;
			}
		}
		self.any & bit != 0
	}
}

/// The amount a water cell contributes to shared surface-corner
/// heights. Falling cells don't surface at their nominal (full)
/// amount: the sheet's lip continues the feeder's downward slope, one
/// level below the highest adjacent resting water — a source feeds the
/// tallest lip there is short of full, a thin flow a thinner one —
/// instead of popping back up to full height at the edge. No feeder in
/// reach (e.g. a floating column) falls back to the nominal amount.
fn surface_amount(input: &MeshInput, x: i32, y: i32, z: i32, b: BlockType) -> Option<u8> {
	let a = b.water_amount()?;
	if !b.is_falling() {
		return Some(a);
	}
	let mut feeder = 0u8;
	for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
		let (nx, nz) = (x + dx, z + dz);
		// Corner cells sit on the padded shell; their neighbors can
		// leave the snapshot — skip those.
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

/// Water surface drop code at horizontal lattice corner (lx, lz) for water
/// cells at height y: the corner touches up to four cells. In priority order —
/// water at y + 1 or a covered water cell in any touching column pins the
/// surface full; else an open-air cell with water directly below plunges the
/// whole block, so cascades render as connected ramps rather than stacked
/// slivers, UNLESS a resting SOURCE touches the corner, since a full block must
/// keep looking full exactly where it spills outward; else the whole-block code
/// minus the STRONGEST touching amount.
///
/// Strongest, not the mean: the sim halves the amount on every spread, so
/// averaging drags every corner toward the thinner water beyond it.
///
/// A pure function of the CORNER, never of the cell emitting the face — which
/// is what lets adjacent cells knit without cracks, chunk borders included.
fn corner_drop(input: &MeshInput, lx: i32, y: i32, lz: i32) -> u32 {
	let mut best = 0u8;
	let mut plunge = false;
	let mut source = false;
	for cz in lz - 1..=lz {
		for cx in lx - 1..=lx {
			// Cell y is inside the chunk, so y ± 1 stays in the shell.
			let above = BlockType::from_id(input.get(cx, y + 1, cz));
			if above.is_water() {
				return 0;
			}
			let b = BlockType::from_id(input.get(cx, y, cz));
			// A covered water cell (non-air above) renders as a full
			// cube (`WaterState::full`); pin the shared corner so open
			// surfaces rise to meet it instead of dipping into a
			// see-through sliver at its edge.
			if b.is_water() && above.props().class != RenderClass::None {
				return 0;
			}
			source |= b.is_fluid_source();
			match surface_amount(input, cx, y, cz, b) {
				Some(a) => best = best.max(a),
				None if b.props().class == RenderClass::None => {
					plunge |= BlockType::from_id(input.get(cx, y - 1, cz)).is_water();
				}
				None => {}
			}
		}
	}
	// A source outranks the plunge: spilling is *exactly* when a diagonal
	// neighbour is air over the water just poured, and a source ringed by
	// that (a bucket emptied against a wall face) plunged all four
	// corners, lost its top face, and had every side skipped as
	// zero-area — the block you just placed rendered nothing at all.
	if plunge && !source {
		return 9;
	}
	debug_assert!(best > 0, "corner of a water face touches its own cell");
	9 - u32::from(best)
}

/// A fall's landing: falling water whose below-cell is where the fall ends —
/// resting water (worldgen falls land in pools) or solid ground (fluid-sim
/// falls punching to a floor).
///
/// Called from `mesh_chunk`'s block loop as well as from the splash ring scan,
/// because recording the impact is a property of the CELL and not of any face
/// it happens to emit. Inside `WaterState::of` it only ran when a face survived
/// culling — and an impact cell is the one water cell most likely to have every
/// face culled, capped by the fall above and ringed by the pool it just made.
pub(super) fn is_impact(input: &MeshInput, x: i32, y: i32, z: i32) -> bool {
	if !BlockType::from_id(input.get(x, y, z)).is_falling() {
		return false;
	}
	let below_raw = input.get(x, y - 1, z);
	let below = BlockType::from_id(below_raw);
	!below.is_falling() && (below.is_water() || occludes(below_raw))
}

/// Per-face water surface state, computed once per cell in `mesh_chunk`.
#[derive(Clone, Copy)]
pub(super) struct WaterState {
	/// Anything but open air directly above: render a complete cube. A
	/// surface only exists under air — water above means submerged, and
	/// an opaque or cutout block above (cave ceiling, the wall over a
	/// dug hole) occludes the top face, so dropped side tops would open
	/// a see-through gap with no ramp covering it (the flooded-cave
	/// missing-face bug: a full corner plunge then skipped the sides as
	/// zero-area and the whole cell vanished). The top block of a fall
	/// column has air above and takes shared corner heights like any
	/// surface cell, so the lip knits with the pool feeding it.
	pub(super) full: bool,
	pub(super) falling: bool,
	/// Horizontal flow direction code: still, or one of the four
	/// horizontal sides in +x, −x, +z, −z order — the side with the least
	/// water, where in-transit water is headed.
	pub(super) flow: u32,
	/// Splash code: a fall's impact cell (falling water striking the
	/// pool surface or solid ground below) carries the top code, and
	/// surface-water cells step one code down per ring of Chebyshev
	/// distance out from it. The shader reads it as a static foam wash
	/// fading outward from the impact — it is a DISTANCE, not a phase, so
	/// nothing here animates. Zero elsewhere.
	pub(super) splash: u32,
	/// Contiguous falling cells above this one, zero at the sheet top,
	/// capped at what the payload field holds. When the walk hits the
	/// snapshot ceiling with the column still falling, this saturates
	/// rather than reporting the partial count: a fall crossing a vertical
	/// chunk seam whitens a little early (masked by the splash below),
	/// where the partial count would visibly reset the sheet to a glassy
	/// lip mid-fall. Exact depth would need depth-encoded block variants.
	pub(super) fall_dist: u32,
}

impl WaterState {
	/// Outlined: inlining this (and emit_water_face) into mesh_chunk's
	/// block loop measurably slowed the dominant solid path (~+0.03ms on
	/// a 0.08ms avg mesh job) — water cells are the rare case.
	#[inline(never)]
	pub(super) fn of(
		input: &MeshInput,
		falls: &mut FallLayers,
		x: i32,
		y: i32,
		z: i32,
		me: BlockType,
	) -> WaterState {
		let falling = me.is_falling();
		let above = BlockType::from_id(input.get(x, y + 1, z));
		let full = above.props().class != RenderClass::None;
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
		let splash = if falling && is_impact(input, x, y, z) {
			3
		} else if !falling && !full && falls.reachable(input, y) {
			// Ring scan: nearest impact in the neighborhood at this
			// level or one above (worldgen falls land one cell above
			// the pool surface), scored a code lower per ring of
			// Chebyshev distance out. An impact far enough into a
			// neighbor chunk sits outside the padded snapshot — a
			// notch in the ring.
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
					if code > best
						&& (is_impact(input, nx, y, nz) || is_impact(input, nx, y + 1, nz))
					{
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
			if cy > 16 { 15 } else { d.min(15) }
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

	/// Payload bits above the per-vertex drop code (pre-shift: the whole
	/// payload lands in word 1 above the texture layer).
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
	// A side face whose whole top edge plunged the full block collapsed
	// onto its bottom edge — zero area, skip it.
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
			(cx as u32 * 12, cy as u32 * 12, cz as u32 * 12),
			dir,
			ci as u32,
		));
		out.push(tex | ((d | state.flags() | (vfall << 10)) << 10));
	}
}
