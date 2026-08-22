//! What an item looks like, as geometry — the one authority shared by the 2D
//! inventory icons (`ui/shaped_icon.rs`, which keeps the three camera-facing
//! quads and projects them dimetrically) and the 3D first-person viewmodel
//! (`viewmodel.rs`, which needs all six).
//!
//! Blocks project the SAME baked `shapes` templates the world renders, so icon,
//! hand and world can never disagree about geometry. The water tool has no block
//! behind it, so its bucket is authored here as a sub-box list in the same
//! twelfths — as boxes rather than loose faces precisely so it is solid from
//! every angle, since the hand turns it and the icon never did. It is the one
//! item with a STATE: charged or empty, which is geometry rather than a tint,
//! because it decides what both mouse buttons do.
//!
//! Everything is pure integer twelfths; f32 appears only in UVs.

use digblock_core::block::BlockType;
use digblock_core::dir::{DIRS, Dir};
use digblock_core::shapes::{self, Facing, door_state, slab_state, stair_state};

use crate::ui::hud::IconKind;

/// One face to draw: geometry in twelfths (y may run past a single cell
/// for the door's stacked halves), UVs already 0..1.
pub(crate) struct ItemFace {
	pub dir: Dir,
	pub corners: [[i32; 3]; 4],
	pub uv: [[f32; 2]; 4],
	pub tex_layer: u16,
	/// Draw this face as fullbright ink, whatever the light is.
	///
	/// Per FACE, and that is the whole reason it exists. A block says
	/// this once for its whole self (`glows`), which is right for a torch
	/// — one material, and a post authored dark enough to take the gain
	/// (R29). A tool is not one material: it is a body worth shading with
	/// one lit part set into it, and a per-draw flag can only choose
	/// between a tool with no form and a glow that isn't one.
	pub emissive: bool,
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
		emissive: false,
	}
}

/// All six faces of an axis box, one texture per face direction (cubes
/// pick top/side; the bucket passes one layer).
fn box_faces(min: [i32; 3], max: [i32; 3], tex_of: impl Fn(Dir) -> u16, out: &mut Vec<ItemFace>) {
	for dir in DIRS {
		out.push(box_face(dir, min, max, tex_of(dir)));
	}
}

/// The same box, drawn as its own light: every face fullbright.
///
/// The layer passed here is also the tool row's texture, which is where
/// the GI reads the colour of the light the tool casts — so the glow you
/// see and the glow it throws are one authored thing.
fn glow_faces(min: [i32; 3], max: [i32; 3], layer: u16, out: &mut Vec<ItemFace>) {
	for dir in DIRS {
		let mut f = box_face(dir, min, max, layer);
		f.emissive = true;
		out.push(f);
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
			// A block that glows says so for its whole self, and the draw
			// carries that flag already (`icon_block`'s `emissive`). The
			// per-face bit is for items with no block behind them.
			emissive: false,
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
	// Planed timber, not the log's bark: a pail is a made thing. This is
	// the one place a layer is named directly rather than taken from a
	// block, because the bucket is an item and has no block to ask.
	let wood = digblock_core::block::tex::LUMBER_LIGHT;
	let water = BlockType::Water.props().tex_side;
	// Base plate, then thin walls. They used to be thicker, and the narrow
	// opening that left made the charge a speck at hotbar-icon size — the
	// wider mouth shows far more water for a sliver of pail.
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

/// The Essence Siphon: a funnel that swallows the rock face, a neck rising out
/// of it, and the glowing vessel the drawn essence collects in, capped with a
/// collar.
///
/// **The taper is the item.** A flat plate, a straight stem and a cube on top
/// read as a rubber stamp: three slabs of decreasing width say "pressed onto
/// something", while a cone says "drawing out of it". The funnel is stepped
/// rather than smooth because everything here is axis boxes in twelfths — at the
/// size this is ever seen, three steps read as a taper.
///
/// The vessel is what makes it name itself at hotbar size, and its glow is
/// authored: `SIPHON_GLOW` is the siphon row's own texture, so the light this
/// tool casts is the colour of the thing you can see it collecting.
fn siphon_faces(out: &mut Vec<ItemFace>) {
	let metal = digblock_core::block::tex::METAL;
	let grip = digblock_core::block::tex::LUMBER_DARK;
	let glow = digblock_core::block::tex::SIPHON_GLOW;
	// A small foot and a stem, not a wide mouth. A funnel hood was tried
	// first, on the reasoning that a cone says "draws out of" — and it
	// cannot, in this projection: both views of an item look DOWN at it,
	// so a downward cone shows only the tops of its rings and reads as
	// the flat pad of a rubber stamp, which is exactly what it was
	// called.
	box_faces([5, 0, 5], [7, 1, 7], |_| metal, out);
	box_faces([5, 1, 5], [7, 3, 7], |_| metal, out);
	// The barrel is the item: a tall column of what has been drawn up
	// into it, and the one part that has to survive being an inch tall in
	// a hotbar slot. Vertical mass reads at any angle; a base plate
	// never did.
	glow_faces([4, 3, 4], [8, 9, 8], glow, out);
	// The spout, out of the barrel's side and stepping DOWN to its mouth.
	// This is what keeps a lit vessel from reading as a lantern, and it
	// has to be on the side: the bottom of an item is the one place
	// neither the hand nor the icon can see.
	box_faces([2, 4, 5], [4, 6, 7], |_| metal, out);
	box_faces([0, 2, 5], [2, 5, 7], |_| metal, out);
	// Collar and grip. The grip is proud of the barrel and dark against
	// it, so the tool has a handle end and a business end — and the
	// silhouette says which way round it is held.
	box_faces([4, 9, 4], [8, 10, 8], |_| metal, out);
	box_faces([3, 10, 3], [9, 12, 9], |_| grip, out);
}

/// The Resonant Pick: a timber haft with a broad metal-dark head, and a
/// resonating core at the join.
///
/// The head is WIDE rather than pointed on purpose — the tool breaks a
/// PLANE, and a narrow pick would promise a bore. The lit core is the
/// same trick the siphon plays: at hotbar size a silhouette needs one
/// bright thing to be recognisable at all — and here it is the tool's
/// own light, `PICK_GLOW` being the pick row's texture too, so what
/// resonates and what it lights the tunnel with are the same blue.
fn pick_faces(out: &mut Vec<ItemFace>) {
	let haft = digblock_core::block::tex::LUMBER_LIGHT;
	let head = digblock_core::block::tex::LUMBER_DARK;
	let core = digblock_core::block::tex::PICK_GLOW;
	box_faces([5, 0, 5], [7, 9, 7], |_| haft, out);
	box_faces([1, 9, 5], [11, 11, 7], |_| head, out);
	glow_faces([5, 8, 4], [7, 10, 8], core, out);
}

/// The Master's Sigil: a crystal set in a standing metal plate on a
/// small foot. It DOES nothing — an Aether price for the last line of
/// the catalog — so it is drawn as a thing you display rather than a
/// thing you swing: an emblem, face-on, with the gem proud of the plate
/// on both sides so the shape reads at hotbar size.
///
/// It needs authored geometry for the same reason its siblings do: a
/// tool carries no texture layers, so the cube fallback would draw the
/// game's most expensive item as a block of whatever sits at layer 0.
fn sigil_faces(out: &mut Vec<ItemFace>) {
	let frame = digblock_core::block::tex::METAL;
	let gem = digblock_core::block::tex::CRYSTAL;
	box_faces([4, 0, 4], [8, 2, 8], |_| frame, out);
	box_faces([2, 2, 5], [10, 10, 7], |_| frame, out);
	box_faces([4, 4, 4], [8, 8, 8], |_| gem, out);
}

/// Total geometry height in twelfths — doors stack two cells, and the
/// icon projector sizes its rect from this.
pub(crate) fn height(kind: IconKind) -> i32 {
	match kind {
		IconKind::Shaped(b) if b.is_door() => 24,
		_ => 12,
	}
}

/// Every face of `kind`, unculled and unsorted. Callers filter (the
/// icon keeps the three visible directions) or draw the lot (the
/// viewmodel, which turns the item).
pub(crate) fn faces(kind: IconKind) -> Vec<ItemFace> {
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
				shape_faces(b, slab_state(false, false), 0, &mut out);
			} else if b.takes_facing() {
				// Stairs and furniture with a front, turned the same way
				// so the icon shows the piece three-quarters on.
				shape_faces(b, stair_state(Facing::NegX), 0, &mut out);
			} else {
				// Stateless shaped items — the torch, the table.
				shape_faces(b, 0, 0, &mut out);
			}
		}
		IconKind::WaterTool { full } => bucket_faces(full, &mut out),
		IconKind::Siphon => siphon_faces(&mut out),
		IconKind::Pick => pick_faces(&mut out),
		IconKind::Sigil => sigil_faces(&mut out),
		// A chip is a sprite, never geometry: `hud::draw_icon` routes it
		// to the flat path before this is asked.
		IconKind::Essence(_) => {}
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
			IconKind::Shaped(BlockType::LumberDoor),
			IconKind::Shaped(BlockType::Chair),
			IconKind::Shaped(BlockType::Table),
			IconKind::Shaped(BlockType::Bed),
			IconKind::WaterTool { full: true },
			IconKind::WaterTool { full: false },
			IconKind::Siphon,
			IconKind::Pick,
			IconKind::Sigil,
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
		let wood = digblock_core::block::tex::LUMBER_LIGHT;
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
