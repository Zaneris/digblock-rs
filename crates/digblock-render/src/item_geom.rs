//! What an item looks like, as geometry — the one authority shared by
//! the 2D inventory icons (`ui/shaped_icon.rs`, which keeps the three
//! camera-facing quads and projects them dimetrically) and the 3D
//! first-person viewmodel (`viewmodel.rs`, which needs all six).
//!
//! Blocks project the SAME baked `shapes` templates the world renders,
//! so icon, hand and world can never disagree about geometry. The water
//! tool has no block behind it, so its bucket is authored here as a
//! sub-box list in the same twelfths — and authored as boxes rather
//! than as loose faces precisely so it is solid from every angle: the
//! hand turns it, the icon never did. It is the one item with a STATE:
//! charged or empty, which is geometry (a source in the pail) rather
//! than a tint, because it decides what both mouse buttons do.
//!
//! Everything is pure integer twelfths; f32 appears only in UVs.

use digblock_core::block::BlockType;
use digblock_core::dir::{Dir, DIRS};
use digblock_core::shapes::{self, door_state, slab_state, stair_state, Facing};

use crate::ui::hud::IconKind;

/// One face to draw: geometry in twelfths (y may exceed 12 for the
/// door's stacked halves), UVs already 0..1.
pub struct ItemFace {
	pub dir: Dir,
	pub corners: [[i32; 3]; 4],
	pub uv: [[f32; 2]; 4],
	pub tex_layer: u16,
}

/// An axis-aligned face of a box. Corners follow `mesher::face_corners`
/// EXACTLY — (TL, TR, BL, BR) as that table orders them — because the
/// baked `shapes` quads next to these obey it too, and both are drawn
/// through `mesher::quad_indices`' pattern. Get it wrong and back-face
/// culling keeps the half of the item you can't see.
///
/// UVs are proportional to the covered span, so a partial face samples
/// the matching window of its texture rather than stretching the whole
/// thing across a sliver.
fn box_face(dir: Dir, min: [i32; 3], max: [i32; 3], tex_layer: u16) -> ItemFace {
	// (lo, hi) planes of the box, addressed the way `face_corners`
	// addresses the (0, 1) planes of a unit cell.
	let (m, x) = (min, max);
	let corners = match dir {
		Dir::West => [
			[x[0], x[1], x[2]],
			[x[0], x[1], m[2]],
			[x[0], m[1], x[2]],
			[x[0], m[1], m[2]],
		],
		Dir::East => [
			[m[0], x[1], m[2]],
			[m[0], x[1], x[2]],
			[m[0], m[1], m[2]],
			[m[0], m[1], x[2]],
		],
		Dir::North => [
			[m[0], x[1], x[2]],
			[x[0], x[1], x[2]],
			[m[0], m[1], x[2]],
			[x[0], m[1], x[2]],
		],
		Dir::South => [
			[x[0], x[1], m[2]],
			[m[0], x[1], m[2]],
			[x[0], m[1], m[2]],
			[m[0], m[1], m[2]],
		],
		Dir::Top => [
			[x[0], x[1], x[2]],
			[m[0], x[1], x[2]],
			[x[0], x[1], m[2]],
			[m[0], x[1], m[2]],
		],
		Dir::Bottom => [
			[x[0], m[1], m[2]],
			[m[0], m[1], m[2]],
			[x[0], m[1], x[2]],
			[m[0], m[1], x[2]],
		],
	};
	// Per-face texture frame, applied to each corner so UVs can't drift
	// out of step with the order above.
	let project = |c: [i32; 3]| {
		let (u, v) = match dir {
			Dir::West => (12 - c[2], 12 - c[1]),
			Dir::East => (c[2], 12 - c[1]),
			Dir::North => (c[0], 12 - c[1]),
			Dir::South => (12 - c[0], 12 - c[1]),
			Dir::Top => (c[0], c[2]),
			Dir::Bottom => (c[0], 12 - c[2]),
		};
		[u as f32 / 12.0, v as f32 / 12.0]
	};
	ItemFace {
		dir,
		uv: corners.map(project),
		corners,
		tex_layer,
	}
}

/// All six faces of an axis box, one texture per face direction (cubes
/// pick top/side; the bucket passes one layer).
fn box_faces(min: [i32; 3], max: [i32; 3], tex_of: impl Fn(Dir) -> u16, out: &mut Vec<ItemFace>) {
	for dir in DIRS {
		out.push(box_face(dir, min, max, tex_of(dir)));
	}
}

/// A baked shape variant's quads (canonical `Facing::NegX`, so the
/// interesting faces point at the camera), `y_off` in twelfths for the
/// door's stacked upper half.
fn shape_faces(b: BlockType, state: u8, y_off: i32, out: &mut Vec<ItemFace>) {
	let Some(variant) = shapes::shape_of(b, state) else {
		return;
	};
	for q in &variant.quads {
		out.push(ItemFace {
			dir: q.dir,
			corners: q
				.corners
				.map(|c| [c[0] as i32, c[1] as i32 + y_off, c[2] as i32]),
			uv: q.uv.map(|c| [c[0] as f32 / 12.0, c[1] as f32 / 12.0]),
			tex_layer: b.texture(q.dir) as u16,
		});
	}
}

/// The water tool: a little bucket — a wooden base plate, four walls
/// standing on it, and (only when charged) a source sitting inside.
///
/// The base is what makes the empty state readable: with walls alone you
/// looked straight through the opening and out the bottom, and an empty
/// bucket has nothing else to fill the hole. It also means the charge is
/// a real difference in the silhouette's interior rather than a tint.
fn bucket_faces(full: bool, out: &mut Vec<ItemFace>) {
	let wood = BlockType::Wood.props().tex_side;
	let water = BlockType::Water.props().tex_side;
	// Base plate, then walls one twelfth thick. The walls were two, and
	// the 4×4 opening that left made the charge a speck at hotbar-icon
	// size — the wider mouth more than doubles the water you can see, for
	// one twelfth of pail.
	box_faces([2, 1, 2], [10, 2, 10], |_| wood, out);
	for (min, max) in [
		([2, 2, 2], [3, 9, 10]),  // -x wall
		([9, 2, 2], [10, 9, 10]), // +x wall
		([3, 2, 2], [9, 9, 3]),   // -z wall
		([3, 2, 9], [9, 9, 10]),  // +z wall
	] {
		box_faces(min, max, |_| wood, out);
	}
	// The source, one twelfth below the rim; its own box, so the bucket
	// reads as a bucket from underneath too.
	if full {
		box_faces([3, 2, 3], [9, 8, 9], |_| water, out);
	}
}

/// Total geometry height in twelfths — doors stack two cells, and the
/// icon projector sizes its rect from this.
pub fn height(kind: IconKind) -> i32 {
	match kind {
		IconKind::Shaped(b) if b.is_door() => 24,
		_ => 12,
	}
}

/// Every face of `kind`, unculled and unsorted. Callers filter (the
/// icon keeps the three visible directions) or draw the lot (the
/// viewmodel, which turns the item).
pub fn faces(kind: IconKind) -> Vec<ItemFace> {
	let mut out = Vec::with_capacity(6 * shapes::MAX_SHAPE_QUADS);
	match kind {
		IconKind::Cube(b) => box_faces([0, 0, 0], [12, 12, 12], |d| b.texture(d) as u16, &mut out),
		IconKind::Shaped(b) => {
			if b.is_door() {
				shape_faces(b, door_state(Facing::NegX, false, false), 0, &mut out);
				shape_faces(b, door_state(Facing::NegX, false, true), 12, &mut out);
			} else if b.is_slab() {
				// The 1/3 variant: that IS the item. A placed slab starts
				// at a third and only grows by stacking more on top, so a
				// 2/3 icon advertised a stack you don't have.
				shape_faces(b, slab_state(false), 0, &mut out);
			} else {
				shape_faces(b, stair_state(Facing::NegX), 0, &mut out);
			}
		}
		IconKind::WaterTool { full } => bucket_faces(full, &mut out),
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Both triangles of every face must wind counter-clockwise as seen
	/// from OUTSIDE the box, under `mesher::quad_indices`' [3,1,0,3,0,2]
	/// pattern — that is exactly the contract `cull: Back` enforces in
	/// the viewmodel pass, and the reason these corners copy
	/// `face_corners` rather than being rederived by eye.
	#[test]
	fn box_faces_wind_outward_under_the_shared_index_pattern() {
		let (min, max) = ([2, 1, 3], [10, 9, 8]);
		let mut out = Vec::new();
		box_faces(min, max, |_| 0, &mut out);
		assert_eq!(out.len(), 6, "a box has six faces");
		let centre = [6.0, 5.0, 5.5];
		for f in &out {
			let p = |i: usize| {
				let c = f.corners[i];
				[c[0] as f32, c[1] as f32, c[2] as f32]
			};
			for tri in [[3, 1, 0], [3, 0, 2]] {
				let (a, b, c) = (p(tri[0]), p(tri[1]), p(tri[2]));
				let (u, v) = (
					[b[0] - a[0], b[1] - a[1], b[2] - a[2]],
					[c[0] - a[0], c[1] - a[1], c[2] - a[2]],
				);
				let n = [
					u[1] * v[2] - u[2] * v[1],
					u[2] * v[0] - u[0] * v[2],
					u[0] * v[1] - u[1] * v[0],
				];
				let outward = [a[0] - centre[0], a[1] - centre[1], a[2] - centre[2]];
				let dot = n[0] * outward[0] + n[1] * outward[1] + n[2] * outward[2];
				assert!(
					dot > 0.0,
					"{:?} tri {tri:?} faces inward (dot {dot})",
					f.dir
				);
			}
		}
	}

	/// The corner order is not a local convention: the mesher's is the
	/// one every quad in the engine obeys, and shapes' baked templates
	/// arrive already in it.
	#[test]
	fn box_corners_match_the_meshers_face_corners() {
		for dir in DIRS {
			let f = box_face(dir, [0, 0, 0], [1, 1, 1], 0);
			let want = digblock_core::mesher::face_corners(0, 0, 0, dir);
			let got: Vec<(i32, i32, i32)> = f.corners.iter().map(|c| (c[0], c[1], c[2])).collect();
			assert_eq!(got, want.to_vec(), "{dir:?}");
		}
	}

	#[test]
	fn every_kind_is_closed_and_inside_its_box() {
		let kinds = [
			IconKind::Cube(BlockType::Dirt),
			IconKind::Shaped(BlockType::WoodStairs),
			IconKind::Shaped(BlockType::WoodSlab),
			IconKind::Shaped(BlockType::WoodDoor),
			IconKind::WaterTool { full: true },
			IconKind::WaterTool { full: false },
		];
		for kind in kinds {
			let f = faces(kind);
			assert!(!f.is_empty(), "{kind:?} drew nothing");
			let h = height(kind);
			for face in &f {
				for c in face.corners {
					assert!((0..=12).contains(&c[0]), "{kind:?} x {c:?}");
					assert!((0..=h).contains(&c[1]), "{kind:?} y {c:?}");
					assert!((0..=12).contains(&c[2]), "{kind:?} z {c:?}");
				}
			}
			// Every direction appears: a hand turns the item, so a
			// silhouette authored from one viewpoint would show holes.
			for dir in DIRS {
				assert!(
					f.iter().any(|q| q.dir == dir),
					"{kind:?} has no {dir:?} face"
				);
			}
		}
	}

	/// An empty bucket is wood and nothing else — the charge is the only
	/// difference between the two states, and it has to be visible.
	#[test]
	fn an_empty_bucket_holds_no_water() {
		let water = BlockType::Water.props().tex_side;
		let f = faces(IconKind::WaterTool { full: false });
		assert!(
			!f.iter().any(|q| q.tex_layer == water),
			"an empty bucket drew water"
		);
		// The base plate is what stops you seeing through the opening
		// once the water that used to fill it is gone.
		let floor = f
			.iter()
			.filter(|q| q.dir == Dir::Top)
			.flat_map(|q| q.corners.iter().map(|c| c[1]))
			.min()
			.unwrap();
		assert!(
			floor <= 2,
			"no floor under the opening (lowest top {floor})"
		);
	}

	#[test]
	fn the_bucket_is_wood_around_water() {
		let f = faces(IconKind::WaterTool { full: true });
		let wood = BlockType::Wood.props().tex_side;
		let water = BlockType::Water.props().tex_side;
		assert!(f.iter().any(|q| q.tex_layer == wood));
		assert!(f.iter().any(|q| q.tex_layer == water));
		// The water surface sits below the rim, never above it.
		let rim = f
			.iter()
			.filter(|q| q.tex_layer == wood)
			.flat_map(|q| q.corners.iter().map(|c| c[1]))
			.max()
			.unwrap();
		let surface = f
			.iter()
			.filter(|q| q.tex_layer == water)
			.flat_map(|q| q.corners.iter().map(|c| c[1]))
			.max()
			.unwrap();
		assert!(surface < rim, "water {surface} should sit under rim {rim}");
	}
}
