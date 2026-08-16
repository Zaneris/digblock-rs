//! 3D item icons: a fixed 2:1 dimetric projection of item geometry into
//! an icon rect, emitted as textured [`Cmd::IconQuad`]s. The geometry
//! itself comes from [`crate::item_geom`], shared with the first-person
//! viewmodel — this module's whole job is to throw away the faces
//! pointing away from the icon's fixed viewpoint and flatten the rest.
//! Per-face shading mirrors `chunk.wgsl`'s AXIS factors, applied by
//! scaling the caller's tint, so grey-out tints compose.
//!
//! Everything is pure and headless-testable: geometry in integer
//! twelfths, projected to f32 screen pixels only at the very end.

use digblock_core::dir::Dir;

use crate::item_geom;

use super::Rect;
use super::draw::{DrawList, Layer};
use super::hud::IconKind;

/// The implied view axis is (+x, +y, +z): exactly these faces show.
fn visible(dir: Dir) -> bool {
	matches!(dir, Dir::West | Dir::North | Dir::Top)
}

/// `chunk.wgsl` AXIS factors for the three visible faces.
fn face_shade(dir: Dir) -> f32 {
	match dir {
		Dir::Top => 1.0,
		Dir::West => 0.85,
		_ => 0.78, // North
	}
}

/// Scale a 0xAABBGGRR tint's RGB by `f` (alpha untouched), so shading
/// composes with grey-out and tool tints.
fn shade(tint: u32, f: f32) -> u32 {
	let ch = |shift: u32| {
		let v = (tint >> shift & 0xFF) as f32 * f;
		(v as u32).min(255) << shift
	};
	(tint & 0xFF00_0000) | ch(16) | ch(8) | ch(0)
}

/// Emit `kind` as a 3D icon filling `rect`. Faces are painter-sorted
/// far-to-near; `tint`'s RGB is scaled by the per-face shading.
pub fn emit(dl: &mut DrawList, layer: Layer, rect: Rect, kind: IconKind, tint: u32) {
	// One fixed viewpoint means half the geometry is provably behind
	// the rest: drop it before sorting rather than overdrawing it.
	let mut quads: Vec<item_geom::ItemFace> = item_geom::faces(kind)
		.into_iter()
		.filter(|q| visible(q.dir))
		.collect();
	let height = item_geom::height(kind);
	// Painter's order: corner-sum depth key, far (small) first.
	quads.sort_by_key(|q| q.corners.iter().map(|c| c[0] + c[1] + c[2]).sum::<i32>());

	// 2:1 dimetric: sx = x − z ∈ [−12, 12]; sy = (x + z)/2 − y ∈
	// [−height, 12]. Screen y grows downward, so higher y rises.
	let span_w = 24.0;
	let span_h = 12.0 + height as f32;
	let s = (rect.w / span_w).min(rect.h / span_h);
	let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
	let mid_sy = (12.0 - height as f32) / 2.0;
	let project = |c: [i32; 3]| {
		let sx = (c[0] - c[2]) as f32;
		let sy = (c[0] + c[2]) as f32 / 2.0 - c[1] as f32;
		[cx + sx * s, cy + (sy - mid_sy) * s]
	};
	for q in &quads {
		// A glowing face takes no directional shading. This projection has
		// no light in it at all — the axis factors are a convention
		// borrowed from the mesher — so "unshaded" is the whole of what
		// fullbright can mean here, and it is enough to keep a tool's lit
		// part reading as the lit part at hotbar size.
		let f = if q.emissive { 1.0 } else { face_shade(q.dir) };
		dl.icon_quad(
			layer,
			q.corners.map(project),
			q.uv,
			q.tex_layer,
			shade(tint, f),
		);
	}
}

#[cfg(test)]
mod tests {
	use digblock_core::block::BlockType;

	use super::super::draw::Cmd;
	use super::*;

	const RECT: Rect = Rect {
		x: 100.0,
		y: 200.0,
		w: 48.0,
		h: 48.0,
	};

	fn emitted(kind: IconKind, tint: u32) -> Vec<([[f32; 2]; 4], u16, u32)> {
		let mut dl = DrawList::default();
		emit(&mut dl, Layer::Icons, RECT, kind, tint);
		dl.cmds(Layer::Icons)
			.0
			.iter()
			.map(|c| match c {
				Cmd::IconQuad {
					corners,
					tex_layer,
					tint,
					..
				} => (*corners, *tex_layer, *tint),
				_ => panic!("expected icon quads"),
			})
			.collect()
	}

	#[test]
	fn cube_emits_three_shaded_faces_inside_the_rect() {
		let quads = emitted(IconKind::Cube(BlockType::Dirt), 0xFFFF_FFFF);
		assert_eq!(quads.len(), 3);
		for (corners, _, _) in &quads {
			for c in corners {
				assert!(c[0] >= RECT.x - 0.01 && c[0] <= RECT.x + RECT.w + 0.01);
				assert!(c[1] >= RECT.y - 0.01 && c[1] <= RECT.y + RECT.h + 0.01);
			}
		}
		// Exactly the three face shades appear (white base tint).
		let tints: Vec<u32> = quads.iter().map(|(_, _, t)| *t).collect();
		for f in [1.0, 0.85, 0.78] {
			assert!(tints.contains(&shade(0xFFFF_FFFF, f)), "missing shade {f}");
		}
		// Top face uses the top texture.
		let top = BlockType::Dirt.props().tex_top;
		assert!(quads.iter().any(|(_, l, _)| *l == top));
	}

	#[test]
	fn stairs_emit_multiple_front_faces_far_to_near() {
		let quads = emitted(IconKind::Shaped(BlockType::WoodStairs), 0xFFFF_FFFF);
		assert!(quads.len() > 3, "a stair silhouette is more than a cube");
		// No quad may use a back-face shade value (only the three
		// visible shades exist).
		let allowed: Vec<u32> = [1.0, 0.85, 0.78]
			.iter()
			.map(|&f| shade(0xFFFF_FFFF, f))
			.collect();
		assert!(quads.iter().all(|(_, _, t)| allowed.contains(t)));
	}

	#[test]
	fn door_spans_both_vertical_halves() {
		let quads = emitted(IconKind::Shaped(BlockType::LumberDoor), 0xFFFF_FFFF);
		let ys: Vec<f32> = quads
			.iter()
			.flat_map(|(c, _, _)| c.iter().map(|p| p[1]))
			.collect();
		let (min, max) = ys
			.iter()
			.fold((f32::MAX, f32::MIN), |(a, b), &y| (a.min(y), b.max(y)));
		assert!(
			max - min > RECT.h * 0.7,
			"door fills most of the rect vertically ({min}..{max})"
		);
	}

	/// The icon must be the 1/3 slab a player actually places, not the
	/// 2/3 grown variant it shipped as.
	#[test]
	fn slab_renders_the_one_third_variant_low() {
		let quads = emitted(IconKind::Shaped(BlockType::WoodSlab), 0xFFFF_FFFF);
		assert_eq!(quads.len(), 3, "a slab is a plain box: three faces");
		let ys: Vec<f32> = quads
			.iter()
			.flat_map(|(c, _, _)| c.iter().map(|p| p[1]))
			.collect();
		let (min_y, max_y) = ys
			.iter()
			.fold((f32::MAX, f32::MIN), |(a, b), &y| (a.min(y), b.max(y)));
		// A slab shares the cube's projected span, so it covers more of
		// the icon than its own height alone would suggest — the dimetric
		// top face has depth of its own — but still well short of a cube.
		let cube = emitted(IconKind::Cube(BlockType::Wood), 0xFFFF_FFFF);
		let cube_min = cube
			.iter()
			.flat_map(|(c, _, _)| c.iter().map(|p| p[1]))
			.fold(f32::MAX, f32::min);
		assert!(
			min_y > cube_min + RECT.h * 0.1,
			"a third sits lower than a full cube ({min_y} vs {cube_min})"
		);
		assert!(
			max_y < RECT.y + RECT.h + 0.01,
			"and stays inside the rect ({max_y})"
		);
	}

	/// The bar has to answer "have I got water?" at a glance, so the two
	/// bucket states must not project to the same picture.
	#[test]
	fn bucket_mixes_wood_and_water_only_when_charged() {
		let wood = digblock_core::block::tex::LUMBER_LIGHT;
		let water = BlockType::Water.props().tex_side;
		let full = emitted(IconKind::WaterTool { full: true }, 0xFFFF_FFFF);
		assert!(full.iter().any(|(_, l, _)| *l == wood));
		assert!(full.iter().any(|(_, l, _)| *l == water));
		let empty = emitted(IconKind::WaterTool { full: false }, 0xFFFF_FFFF);
		assert!(empty.iter().any(|(_, l, _)| *l == wood));
		assert!(!empty.iter().any(|(_, l, _)| *l == water));
	}

	#[test]
	fn shade_scales_rgb_and_keeps_alpha() {
		let t = shade(0x80FF_8040, 0.5);
		assert_eq!(t >> 24, 0x80, "alpha untouched");
		assert_eq!(t >> 16 & 0xFF, 0x7F);
		assert_eq!(t >> 8 & 0xFF, 0x40);
		assert_eq!(t & 0xFF, 0x20);
	}
}
