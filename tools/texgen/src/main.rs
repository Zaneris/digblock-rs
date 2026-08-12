//! Deterministic generator for derived block textures: palette-shift and
//! noise variations of the hand-made originals, so new blocks stay in the
//! established pixel-art style. Run manually (`cargo run -p texgen`) and
//! commit the outputs — assets are `include_bytes!`d, so generated PNGs
//! live in git; this tool is checked in for provenance and regeneration.
//! Output matches the loader contract: 64×64 8-bit RGB, non-interlaced.

use image::RgbImage;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/textures");
const SIZE: u32 = 64;

fn main() {
	let grass_top = load("GrassTop.png");
	let grass_side = load("GrassSide.png");
	let dirt = load("Dirt.png");
	let stone = load("Stone.png");
	let leaves = load("Leaves.png");

	// Dry savanna grass: hue swung from green toward yellow-tan, slightly
	// desaturated and lightened. The side texture keeps its dirt body —
	// only green-dominant pixels (the grass fringe) are transformed.
	save("DryGrassTop.png", map(&grass_top, dry));
	save(
		"DryGrassSide.png",
		map(&grass_side, |p| {
			if p[1] > p[0] && p[1] > p[2] {
				dry(p)
			} else {
				p
			}
		}),
	);

	// Wet dark brown with a low-amplitude sheen.
	let mut rng = Rng(0x00d1_6b10_cc0f_fee5);
	save("Mud.png", mud(&dirt, &mut rng));

	// Stone speckled with lightened/darkened pebble blobs.
	save("Gravel.png", gravel(&stone, &mut rng));

	// Darker blue-green conifer foliage with a touch more contrast.
	save("PineLeaves.png", map(&leaves, pine));

	// Compacted sand with horizontal strata banding.
	let sand = load("Sand.png");
	save("Sandstone.png", sandstone(&sand, &mut rng));

	// Door leaf: wood with vertical plank seams and a slight overall
	// darkening (the window lites are geometry, not texture).
	let wood = load("Wood.png");
	save("DoorWood.png", door(&wood, &mut rng));

	// Broken rock — what mined stone actually drops (SKILLS-DESIGN §7.1):
	// stone body with angular fracture cracks and chipped facets, reading
	// as rough cobble when placed. Appended LAST so the shared rng stream
	// leaves every earlier output byte-identical.
	save("Rubble.png", rubble(&stone, &mut rng));

	// Torch: the side texture maps the post's wood to the LOWER rows and
	// the flame glow to the top rows (a box face's v runs top-down from
	// the cell's top, so the flame head at y 7..10 samples rows 2..5);
	// the flame top face gets its own radial-glow layer.
	save("TorchSide.png", torch_side(&wood, &mut rng));
	save("TorchFlame.png", torch_flame(&mut rng));
}

/// Wood post darkened, with the top ~5/12 of rows turned to flame glow
/// (white-hot core rows fading through yellow to orange where the flame
/// head's side faces sample).
fn torch_side(wood: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = wood.clone();
	let flame_rows = SIZE * 5 / 12; // rows 0..≈26 of 64
	for (_, y, p) in out.enumerate_pixels_mut() {
		if y < flame_rows {
			// 0 at the very top (cool fringe) → 1 at the head's base.
			let t = y as f32 / flame_rows as f32;
			let j = rng.signed(10);
			p.0 = [
				clamp8(255.0 + j as f32),
				clamp8(150.0 + 90.0 * t + j as f32),
				clamp8(30.0 + 60.0 * t),
			];
		} else {
			let g = rng.signed(5);
			for c in p.0.iter_mut() {
				*c = add8(*c, -18 + g);
			}
		}
	}
	out
}

/// The flame head's top: a radial white-yellow glow on ember orange.
fn torch_flame(rng: &mut Rng) -> RgbImage {
	let mut out = RgbImage::new(SIZE, SIZE);
	let c = SIZE as f32 / 2.0;
	for (x, y, p) in out.enumerate_pixels_mut() {
		let d = (((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c).min(1.0);
		let j = rng.signed(8) as f32;
		p.0 = [
			clamp8(255.0 - 30.0 * d + j),
			clamp8(230.0 - 120.0 * d + j),
			clamp8(120.0 - 100.0 * d),
		];
	}
	out
}

/// Fractured stone: slight darkening jitter, crack walks (a dark groove
/// with a light catch edge one pixel down-right), and a few darker
/// chipped facets — all wrapped modularly so the texture keeps tiling.
fn rubble(stone: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = stone.clone();
	for p in out.pixels_mut() {
		let n = rng.signed(6) - 4;
		p.0 = [add8(p.0[0], n), add8(p.0[1], n), add8(p.0[2], n)];
	}
	let s = SIZE as i32;
	const DIRS: [(i32, i32); 8] = [
		(1, 0),
		(1, 1),
		(0, 1),
		(-1, 1),
		(-1, 0),
		(-1, -1),
		(0, -1),
		(1, -1),
	];
	for _ in 0..7 {
		let (mut x, mut y) = (rng.range(SIZE as u64) as i32, rng.range(SIZE as u64) as i32);
		let mut d = rng.range(8) as usize;
		for _ in 0..(24 + rng.range(16)) {
			let p = out.get_pixel_mut(x.rem_euclid(s) as u32, y.rem_euclid(s) as u32);
			p.0 = [add8(p.0[0], -42), add8(p.0[1], -42), add8(p.0[2], -42)];
			let e = out.get_pixel_mut((x + 1).rem_euclid(s) as u32, (y + 1).rem_euclid(s) as u32);
			e.0 = [add8(e.0[0], 12), add8(e.0[1], 12), add8(e.0[2], 12)];
			if rng.range(3) == 0 {
				// ±1 step of eight-way turn keeps cracks angular, not wavy.
				d = (d + 7 + 2 * rng.range(2) as usize) % 8;
			}
			x += DIRS[d].0;
			y += DIRS[d].1;
		}
	}
	for _ in 0..12 {
		let (cx, cy) = (rng.range(SIZE as u64) as i32, rng.range(SIZE as u64) as i32);
		let r = 1 + rng.range(2) as i32;
		let delta = -(8 + rng.signed(6).abs());
		for dy in -r..=r {
			for dx in -r..=r {
				if dx * dx + dy * dy <= r * r {
					let p = out.get_pixel_mut(
						(cx + dx).rem_euclid(s) as u32,
						(cy + dy).rem_euclid(s) as u32,
					);
					p.0 = [
						add8(p.0[0], delta),
						add8(p.0[1], delta),
						add8(p.0[2], delta),
					];
				}
			}
		}
	}
	out
}

/// Vertical plank seams every 16px (dark 2px groove with a light catch
/// edge), a gentle overall darken, and per-pixel grain jitter.
fn door(src: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = src.clone();
	for (x, _y, p) in out.enumerate_pixels_mut() {
		let seam = x % 16;
		let d = match seam {
			0 => -38,
			1 => -22,
			2 => 14, // light catch on the plank edge
			_ => 0,
		};
		let grain = rng.signed(6);
		for c in p.0.iter_mut() {
			*c = add8(*c, d - 10 + grain);
		}
	}
	out
}

fn load(name: &str) -> RgbImage {
	let img = image::open(format!("{DIR}/{name}"))
		.unwrap_or_else(|e| panic!("open {name}: {e}"))
		.to_rgb8();
	assert_eq!(
		(img.width(), img.height()),
		(SIZE, SIZE),
		"{name} must be 64x64"
	);
	img
}

fn save(name: &str, img: RgbImage) {
	img.save(format!("{DIR}/{name}"))
		.unwrap_or_else(|e| panic!("save {name}: {e}"));
	println!("wrote {name}");
}

fn map(src: &RgbImage, f: impl Fn([u8; 3]) -> [u8; 3]) -> RgbImage {
	let mut out = src.clone();
	for p in out.pixels_mut() {
		p.0 = f(p.0);
	}
	out
}

/// Splitmix64 — fixed seed, row-major consumption: byte-identical output
/// on every run.
struct Rng(u64);

impl Rng {
	fn next(&mut self) -> u64 {
		self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
		let mut z = self.0;
		z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
		z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
		z ^ (z >> 31)
	}

	fn range(&mut self, n: u64) -> u64 {
		self.next() % n
	}

	/// Uniform in [-amp, amp].
	fn signed(&mut self, amp: i32) -> i32 {
		self.range(u64::from(2 * amp as u32 + 1)) as i32 - amp
	}
}

fn clamp8(v: f32) -> u8 {
	v.round().clamp(0.0, 255.0) as u8
}

fn add8(v: u8, d: i32) -> u8 {
	(i32::from(v) + d).clamp(0, 255) as u8
}

fn rgb_to_hsv([r, g, b]: [u8; 3]) -> (f32, f32, f32) {
	let (r, g, b) = (
		f32::from(r) / 255.0,
		f32::from(g) / 255.0,
		f32::from(b) / 255.0,
	);
	let max = r.max(g).max(b);
	let min = r.min(g).min(b);
	let d = max - min;
	let h = if d == 0.0 {
		0.0
	} else if max == r {
		60.0 * (((g - b) / d).rem_euclid(6.0))
	} else if max == g {
		60.0 * ((b - r) / d + 2.0)
	} else {
		60.0 * ((r - g) / d + 4.0)
	};
	let s = if max == 0.0 { 0.0 } else { d / max };
	(h, s, max)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
	let h = h.rem_euclid(360.0);
	let c = v * s;
	let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
	let m = v - c;
	let (r, g, b) = match (h / 60.0) as u32 {
		0 => (c, x, 0.0),
		1 => (x, c, 0.0),
		2 => (0.0, c, x),
		3 => (0.0, x, c),
		4 => (x, 0.0, c),
		_ => (c, 0.0, x),
	};
	[
		clamp8((r + m) * 255.0),
		clamp8((g + m) * 255.0),
		clamp8((b + m) * 255.0),
	]
}

/// Green → yellow-tan: hue −30°, saturation ×0.8, value ×1.05.
fn dry(p: [u8; 3]) -> [u8; 3] {
	let (h, s, v) = rgb_to_hsv(p);
	hsv_to_rgb(h - 30.0, s * 0.8, (v * 1.05).min(1.0))
}

/// Darker blue-green with a mild contrast lift — deep enough that pine
/// canopies read distinct from broadleaf at a distance.
fn pine([r, g, b]: [u8; 3]) -> [u8; 3] {
	let contrast = |c: f32| (c - 128.0) * 1.15 + 128.0;
	[
		clamp8(contrast(f32::from(r) * 0.52)),
		clamp8(contrast(f32::from(g) * 0.70)),
		clamp8(contrast(f32::from(b) * 0.66)),
	]
}

fn mud(dirt: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = dirt.clone();
	for p in out.pixels_mut() {
		let n = rng.signed(6);
		p.0 = [
			add8(clamp8(f32::from(p.0[0]) * 0.62), n),
			add8(clamp8(f32::from(p.0[1]) * 0.55), n),
			add8(clamp8(f32::from(p.0[2]) * 0.52), n),
		];
	}
	out
}

fn sandstone(sand: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = sand.clone();
	// Horizontal strata: darken bands of 3–6 rows by a per-band amount,
	// wrapped per-pixel noise keeping the grain. Row phases come from the
	// seeded stream, so regeneration is byte-identical.
	let mut y = 0u32;
	while y < SIZE {
		let band_h = 3 + rng.range(4) as u32;
		let shade = -(4 + rng.range(12) as i32);
		for by in y..(y + band_h).min(SIZE) {
			for x in 0..SIZE {
				let n = rng.signed(5);
				let p = out.get_pixel_mut(x, by);
				// Slight desaturation toward a rockier tan.
				let avg = (i32::from(p.0[0]) + i32::from(p.0[1]) + i32::from(p.0[2])) / 3;
				for c in p.0.iter_mut() {
					let mixed = (i32::from(*c) * 3 + avg) / 4;
					*c = (mixed + shade + n).clamp(0, 255) as u8;
				}
			}
		}
		y += band_h;
	}
	out
}

fn gravel(stone: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = stone.clone();
	for p in out.pixels_mut() {
		let n = rng.signed(8);
		p.0 = [add8(p.0[0], n), add8(p.0[1], n), add8(p.0[2], n)];
	}
	// ~40 pebbles: small lightened/darkened blobs with a darker 1px rim,
	// wrapped modularly so the texture keeps tiling.
	let s = SIZE as i32;
	for _ in 0..40 {
		let (cx, cz) = (rng.range(SIZE as u64) as i32, rng.range(SIZE as u64) as i32);
		let r = 1 + rng.range(2) as i32;
		let sign = if rng.range(2) == 0 { 1 } else { -1 };
		let delta = sign * (10 + rng.range(16) as i32);
		for dy in -r - 1..=r + 1 {
			for dx in -r - 1..=r + 1 {
				let d2 = dx * dx + dy * dy;
				let p = out.get_pixel_mut(
					(cx + dx).rem_euclid(s) as u32,
					(cz + dy).rem_euclid(s) as u32,
				);
				if d2 <= r * r {
					p.0 = [
						add8(p.0[0], delta),
						add8(p.0[1], delta),
						add8(p.0[2], delta),
					];
				} else if d2 <= (r + 1) * (r + 1) {
					p.0 = [add8(p.0[0], -18), add8(p.0[1], -18), add8(p.0[2], -18)];
				}
			}
		}
	}
	out
}
