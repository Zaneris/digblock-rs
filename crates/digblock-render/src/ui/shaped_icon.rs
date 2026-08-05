//! 3D item icons: a fixed 2:1 dimetric projection of block geometry
//! into an icon rect, emitted as textured [`Cmd::IconQuad`]s. Cubes are
//! a static three-face template; stairs and doors project the SAME
//! baked `shapes` templates the world renders (so icon and world can
//! never disagree about geometry); the water tool is a hand-authored
//! little bucket. Per-face shading mirrors `chunk.wgsl`'s AXIS factors,
//! applied by scaling the caller's tint — so grey-out tints compose.
//!
//! Everything is pure and headless-testable: geometry in integer
//! twelfths, projected to f32 screen pixels only at the very end.

use digblock_core::block::BlockType;
use digblock_core::dir::Dir;
use digblock_core::shapes::{self, door_state, stair_state, Facing};

use super::draw::{DrawList, Layer};
use super::hud::IconKind;
use super::Rect;

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

/// One face to project: geometry in twelfths (y may exceed 12 for the
/// door's stacked halves), UVs already 0..1.
struct Q {
	dir: Dir,
	corners: [[i32; 3]; 4],
	uv: [[f32; 2]; 4],
	tex_layer: u16,
}

/// An axis-aligned visible face of a box, UVs proportional to the
/// covered span so partial faces sample the matching texture window.
fn box_face(dir: Dir, min: [i32; 3], max: [i32; 3], tex_layer: u16) -> Q {
	let uv = |a: i32, b: i32| [a as f32 / 12.0, b as f32 / 12.0];
	let (corners, uv) = match dir {
		// +x plane at max[0]; tangent u = z (mirrored), v = -y.
		Dir::West => (
			[
				[max[0], max[1], max[2]],
				[max[0], max[1], min[2]],
				[max[0], min[1], max[2]],
				[max[0], min[1], min[2]],
			],
			[
				uv(12 - max[2], 12 - max[1]),
				uv(12 - min[2], 12 - max[1]),
				uv(12 - max[2], 12 - min[1]),
				uv(12 - min[2], 12 - min[1]),
			],
		),
		// +z plane at max[2]; tangent u = x, v = -y.
		Dir::North => (
			[
				[min[0], max[1], max[2]],
				[max[0], max[1], max[2]],
				[min[0], min[1], max[2]],
				[max[0], min[1], max[2]],
			],
			[
				uv(min[0], 12 - max[1]),
				uv(max[0], 12 - max[1]),
				uv(min[0], 12 - min[1]),
				uv(max[0], 12 - min[1]),
			],
		),
		// +y plane at max[1]; tangent u = x, v = z.
		_ => (
			[
				[min[0], max[1], min[2]],
				[max[0], max[1], min[2]],
				[min[0], max[1], max[2]],
				[max[0], max[1], max[2]],
			],
			[
				uv(min[0], min[2]),
				uv(max[0], min[2]),
				uv(min[0], max[2]),
				uv(max[0], max[2]),
			],
		),
	};
	Q {
		dir,
		corners,
		uv,
		tex_layer,
	}
}

/// The three visible faces of an axis box, one texture per face
/// direction (cubes pick top/side; the bucket passes one layer).
fn box_faces(min: [i32; 3], max: [i32; 3], tex_of: impl Fn(Dir) -> u16, out: &mut Vec<Q>) {
	for dir in [Dir::West, Dir::North, Dir::Top] {
		out.push(box_face(dir, min, max, tex_of(dir)));
	}
}

/// Project a baked shape variant's quads (canonical `Facing::NegX`, so
/// the interesting faces point at the camera), `y_off` in twelfths for
/// the door's stacked upper half.
fn shape_quads(b: BlockType, state: u8, y_off: i32, out: &mut Vec<Q>) {
	let Some(variant) = shapes::shape_of(b, state) else {
		return;
	};
	for q in &variant.quads {
		if !visible(q.dir) {
			continue;
		}
		out.push(Q {
			dir: q.dir,
			corners: q
				.corners
				.map(|c| [c[0] as i32, c[1] as i32 + y_off, c[2] as i32]),
			uv: q.uv.map(|c| [c[0] as f32 / 12.0, c[1] as f32 / 12.0]),
			tex_layer: b.texture(q.dir) as u16,
		});
	}
}

/// The water tool: a little bucket — walls with a rim, water inside.
fn bucket_quads(out: &mut Vec<Q>) {
	let wood = BlockType::Wood.props().tex_side;
	let water = BlockType::Water.props().tex_side;
	// Outer shell (visible west/north walls; the top face is replaced
	// by the rim ring below, so skip it).
	for dir in [Dir::West, Dir::North] {
		out.push(box_face(dir, [2, 1, 2], [10, 9, 10], wood));
	}
	// Rim ring at y = 9: four strips around the opening.
	for (min, max) in [
		([2, 1, 2], [4, 9, 10]),  // -x strip
		([8, 1, 2], [10, 9, 10]), // +x strip
		([4, 1, 2], [8, 9, 4]),   // -z strip
		([4, 1, 8], [8, 9, 10]),  // +z strip
	] {
		out.push(box_face(Dir::Top, min, max, wood));
	}
	// Water surface inside, a little below the rim.
	out.push(box_face(Dir::Top, [4, 1, 4], [8, 8, 8], water));
}

/// Emit `kind` as a 3D icon filling `rect`. Faces are painter-sorted
/// far-to-near; `tint`'s RGB is scaled by the per-face shading.
pub fn emit(dl: &mut DrawList, layer: Layer, rect: Rect, kind: IconKind, tint: u32) {
	let mut quads = Vec::with_capacity(2 * shapes::MAX_SHAPE_QUADS);
	// Geometry height in twelfths (doors stack two cells).
	let mut height = 12;
	match kind {
		IconKind::Cube(b) => {
			box_faces([0, 0, 0], [12, 12, 12], |d| b.texture(d) as u16, &mut quads)
		}
		IconKind::Shaped(b) => {
			if b.is_door() {
				height = 24;
				shape_quads(b, door_state(Facing::NegX, false, false), 0, &mut quads);
				shape_quads(b, door_state(Facing::NegX, false, true), 12, &mut quads);
			} else {
				shape_quads(b, stair_state(Facing::NegX), 0, &mut quads);
			}
		}
		IconKind::WaterTool => bucket_quads(&mut quads),
	}
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
		dl.icon_quad(
			layer,
			q.corners.map(project),
			q.uv,
			q.tex_layer,
			shade(tint, face_shade(q.dir)),
		);
	}
}

#[cfg(test)]
mod tests {
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
		let quads = emitted(IconKind::Shaped(BlockType::WoodDoor), 0xFFFF_FFFF);
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

	#[test]
	fn bucket_mixes_wood_and_water() {
		let quads = emitted(IconKind::WaterTool, 0xFFFF_FFFF);
		let wood = BlockType::Wood.props().tex_side;
		let water = BlockType::Water.props().tex_side;
		assert!(quads.iter().any(|(_, l, _)| *l == wood));
		assert!(quads.iter().any(|(_, l, _)| *l == water));
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
