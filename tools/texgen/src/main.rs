//! Deterministic 64x64 block textures and UI sprites. Run `cargo run -p texgen`
//! and commit the generated PNGs. Surface recipes own independent seeds; live
//! and depleted nodes share geometry. Soil, turf and timber are generated in
//! the same style as the mineral surfaces. Water remains untouched source art.
//! Output is 8-bit RGB or RGBA, matching the texture-array loader contract.

use digblock_core::block::{BlockType as B, Essence};
use image::{RgbImage, RgbaImage};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/textures");
const SIZE: u32 = 64;

mod surfaces;

fn main() {
	use surfaces::Deposit as D;
	let stone = surfaces::stone();
	let sand = surfaces::sand(false);
	let log = surfaces::bark();
	let dirt = surfaces::dirt();
	let grass = surfaces::grass_top(false);
	let dry_grass = surfaces::grass_top(true);
	let lumber_dark = surfaces::lumber([96, 73, 48]);
	for (name, img) in [
		("Dirt.png", dirt.clone()),
		("GrassTop.png", grass.clone()),
		("GrassSide.png", surfaces::grass_side(&dirt, &grass)),
		("DryGrassTop.png", dry_grass.clone()),
		("DryGrassSide.png", surfaces::grass_side(&dirt, &dry_grass)),
		("Log.png", log.clone()),
		("LogTop.png", surfaces::log_top(&log)),
		("LumberLight.png", surfaces::lumber([191, 153, 101])),
		("LumberDark.png", lumber_dark.clone()),
		("LumberRed.png", surfaces::lumber([145, 87, 54])),
		("Stone.png", stone.clone()),
		("Sand.png", sand.clone()),
		("Snow.png", surfaces::snow()),
		("Leaves.png", surfaces::foliage(false)),
		("PineLeaves.png", surfaces::foliage(true)),
		("Mud.png", surfaces::mud()),
		("Gravel.png", surfaces::aggregate(true)),
		("Rubble.png", surfaces::aggregate(false)),
		("Sandstone.png", surfaces::sand(true)),
		("StoneBrick.png", surfaces::bricks(false)),
		("SandstoneBrick.png", surfaces::bricks(true)),
		("PolishedStone.png", surfaces::polished()),
		("Metal.png", surfaces::metal()),
		("DepletedStone.png", surfaces::depleted(&stone)),
		("Lantern.png", surfaces::lamp(false)),
		("Aetherlamp.png", surfaces::lamp(true)),
		("TorchSide.png", surfaces::torch(&lumber_dark)),
		(
			"TorchFlame.png",
			surfaces::glow([255, 230, 120], [225, 110, 20]),
		),
		(
			"SiphonGlow.png",
			surfaces::glow([48, 190, 96], [12, 84, 48]),
		),
		(
			"PickGlow.png",
			surfaces::glow([60, 140, 235], [16, 40, 120]),
		),
	] {
		save(name, img);
	}

	for (live, spent, block, host, kind) in [
		(
			"Emberore.png",
			"DepletedEmberore.png",
			B::Emberore,
			&stone,
			D::Ember,
		),
		(
			"Duskstone.png",
			"DepletedDuskstone.png",
			B::Duskstone,
			&stone,
			D::Dusk,
		),
		(
			"Glowvein.png",
			"DepletedGlowvein.png",
			B::Glowvein,
			&stone,
			D::Glow,
		),
		(
			"Aetherock.png",
			"DepletedAetherock.png",
			B::Aetherock,
			&stone,
			D::Aether,
		),
		(
			"Heartwood.png",
			"DrainedHeartwood.png",
			B::Heartwood,
			&log,
			D::Heart,
		),
		(
			"Leystone.png",
			"SpentLeystone.png",
			B::Leystone,
			&stone,
			D::Ley,
		),
		(
			"Springstone.png",
			"SpentSpringstone.png",
			B::Springstone,
			&sand,
			D::Spring,
		),
	] {
		let (core, rim) = block.node_flecks().expect("node palette");
		let (a, b, emission) = surfaces::deposit(host, core, rim, kind);
		save(live, a);
		save(spent, b);
		save(&live.replace(".png", "Emission.png"), emission);
	}
	save_rgba("Crystal.png", surfaces::crystal());

	// Clean panes retain their transmission; a painted pattern repeats across every spell shell.
	save_rgba(
		"Glass.png",
		RgbaImage::from_pixel(SIZE, SIZE, image::Rgba([210, 228, 236, 120])),
	);
	for (name, tint) in [
		("GlassRed.png", [225, 58, 48]),
		("GlassGreen.png", [84, 205, 96]),
		("GlassBlue.png", [72, 128, 232]),
		("GlassAmber.png", [235, 172, 64]),
	] {
		save_rgba(name, tinted_pane(tint));
	}
	save_rgba("Wardglass.png", wardglass());

	for (name, e) in [
		("EssenceVitae.png", Essence::Vitae),
		("EssenceTerra.png", Essence::Terra),
		("EssenceUnda.png", Essence::Unda),
		("EssenceFerrum.png", Essence::Ferrum),
		("EssenceAether.png", Essence::Aether),
	] {
		save_rgba(name, essence_flake(e));
	}
	save_rgba("SpellWisp.png", pixel_glyph(spell_wisp()));
	save_rgba("SpellKindle.png", pixel_glyph(spell_kindle()));
	save_rgba("SpellSphere.png", pixel_glyph(spell_sphere()));
	save_rgba("SpellCauseway.png", pixel_glyph(spell_causeway()));
	save_rgba("SpellHome.png", pixel_glyph(monolith([150, 150, 158])));
	save_rgba("SpellPort.png", pixel_glyph(monolith([236, 239, 244])));
}

/// A coloured clean pane: the glass look in one saturated tint, alpha
/// high enough that what it passes is unmistakably ITS colour — the
/// stained-glass blocks. Deliberately rng-free (see the call site).
fn tinted_pane(tint: [u8; 3]) -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	for p in out.pixels_mut() {
		p.0 = [tint[0], tint[1], tint[2], 200];
	}
	out
}

/// The causeway's plank: one flat, cool near-white at an alpha well above
/// clear glass's, so a road reads as glass from above and below — and
/// nothing else, since any mark tiles into a pattern across a sphere.
/// Deliberately rng-free (see the call site).
fn wardglass() -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	for p in out.pixels_mut() {
		p.0 = [214, 226, 240, 150];
	}
	out
}

/// A chipped crystal with broad facets and a narrow bevel. Transparent pixels carry the
/// chip's colour so the atlas mip filter cannot introduce a dark fringe.
fn essence_flake(e: Essence) -> RgbaImage {
	let (core, rim) = e.palette();
	let (sx, sy, slant) = match e {
		Essence::Vitae => (0.78, 1.0, 0.25),
		Essence::Terra => (1.0, 0.85, -0.08),
		Essence::Unda => (0.84, 1.0, -0.15),
		Essence::Ferrum => (1.0, 0.78, 0.18),
		Essence::Aether => (0.72, 1.0, -0.22),
	};
	let vertices = [
		(-17.0, -22.0),
		(5.0, -26.0),
		(22.0, -13.0),
		(26.0, 6.0),
		(11.0, 23.0),
		(-8.0, 26.0),
		(-24.0, 9.0),
		(-25.0, -9.0),
	];
	RgbaImage::from_fn(SIZE, SIZE, |x, y| {
		let py = (y as f32 - 31.5) / sy;
		let px = (x as f32 - 31.5) / sx - py * slant;
		let mut distance = f32::MAX;
		let mut edge = 0;
		for i in 0..vertices.len() {
			let (ax, ay) = vertices[i];
			let (bx, by) = vertices[(i + 1) % vertices.len()];
			let (dx, dy): (f32, f32) = (bx - ax, by - ay);
			let d = (dx * (py - ay) - dy * (px - ax)) / (dx * dx + dy * dy).sqrt();
			if d < distance {
				distance = d;
				edge = i;
			}
		}
		let alpha = ((distance + 0.5).clamp(0.0, 1.0) * 255.0).round() as u8;
		let facet = if px + py * 0.6 < -5.0 {
			0.30
		} else if px - py * 0.5 > 8.0 {
			-0.25
		} else {
			0.0
		};
		let lit_edge = matches!(edge, 0 | 6 | 7);
		let light = if distance < 2.2 {
			if lit_edge { 0.70 } else { -0.40 }
		} else {
			facet
		};
		let rgb: [u8; 3] = std::array::from_fn(|i| {
			if light >= 0.0 {
				clamp8(f32::from(core[i]) + (f32::from(rim[i]) - f32::from(core[i])) * light)
			} else {
				clamp8(f32::from(core[i]) * (1.0 + light))
			}
		});
		image::Rgba([rgb[0], rgb[1], rgb[2], alpha])
	})
}

/// A shared two-pixel grid and restrained colour steps for the spell silhouettes.
/// Preserve transparent RGB and sampled alpha, including the original antialiased edges.
fn pixel_glyph(src: RgbaImage) -> RgbaImage {
	RgbaImage::from_fn(SIZE, SIZE, |x, y| {
		let mut p = *src.get_pixel(x / 2 * 2 + 1, y / 2 * 2 + 1);
		for v in &mut p.0[..3] {
			*v = clamp8((f32::from(*v) / 8.0).round() * 8.0);
		}
		p
	})
}

/// The wisp's glyph: a warm mote — a hot core inside a soft glow, with
/// four axis rays in a diamond. Transparent field with the glow's RGB
/// (the essence flake's mip rule). Rng-free.
fn spell_wisp() -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	let c = SIZE as f32 / 2.0;
	let warm = [255.0, 208.0, 122.0];
	let hot = [255.0, 246.0, 216.0];
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
		let d = (dx * dx + dy * dy).sqrt();
		let core = (1.0 - d / (c * 0.40)).clamp(0.0, 1.0);
		// The rays hug an axis and taper toward the rim.
		let hug = (1.0 - dx.abs().min(dy.abs()) / 2.2).clamp(0.0, 1.0);
		let ray = hug * (1.0 - d / (c * 0.94)).clamp(0.0, 1.0).powi(2);
		let a = (core + ray * 0.85).min(1.0);
		let rgb: Vec<u8> = warm
			.iter()
			.zip(hot.iter())
			.map(|(&w, &h)| clamp8(w + (h - w) * core))
			.collect();
		p.0 = [rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8];
	}
	out
}

/// The kindle's glyph: a standing flame teardrop — orange body, bright
/// axis, widest below the middle and drawn to a tip. Rng-free.
fn spell_kindle() -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	let s = SIZE as f32;
	let c = s / 2.0;
	// A droplet: a circle with the two tangent lines drawn up to a tip —
	// rounded base, concave flanks, a fine point.
	let tip = s * 0.06;
	let depth = s * 0.56; // tip to circle centre
	let r = s * 0.27;
	let cy = tip + depth;
	let wedge = r / (depth * depth - r * r).sqrt();
	let body = [255.0, 138.0, 42.0];
	let bright = [255.0, 228.0, 148.0];
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
		let dx = (fx - c).abs();
		let dc = (dx * dx + (fy - cy) * (fy - cy)).sqrt();
		let circle = (r - dc + 0.5).clamp(0.0, 1.0);
		let w_line = (fy - tip) * wedge;
		let in_wedge = if fy > tip && fy < cy {
			(w_line - dx + 0.5).clamp(0.0, 1.0)
		} else {
			0.0
		};
		let a = circle.max(in_wedge);
		// Bright along the axis, body colour at the edge.
		let w_here = if fy < cy { w_line.max(0.5) } else { r };
		let axis = (1.0 - dx / w_here).clamp(0.0, 1.0);
		let rgb: Vec<u8> = body
			.iter()
			.zip(bright.iter())
			.map(|(&b, &h)| clamp8(b + (h - b) * axis.powf(1.6)))
			.collect();
		p.0 = [rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8];
	}
	out
}

/// The sphere's glyph: a hollow shell — a cool ring with a cutaway gap
/// at the upper right and a faint interior, so it reads as a shell
/// rather than a disc. Rng-free.
fn spell_sphere() -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	let c = SIZE as f32 / 2.0;
	let ring_r = c * 0.72;
	let thick = c * 0.16;
	let shell = [168.0, 186.0, 214.0];
	let rim = [224.0, 236.0, 252.0];
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
		let d = (dx * dx + dy * dy).sqrt();
		// The cutaway: a slice of the ring is simply not there.
		let ang = dy.atan2(dx);
		let gap = (-1.35..=-0.35).contains(&ang);
		let on_ring = (thick - (d - ring_r).abs() + 0.5).clamp(0.0, 1.0);
		let interior = if d < ring_r - thick { 0.12 } else { 0.0 };
		let a = if gap { interior } else { on_ring.max(interior) };
		// Lit from the upper left, the flake's convention.
		let facing = if d > 0.0 {
			(-0.6 * dx - 0.8 * dy) / d
		} else {
			0.0
		};
		let lit = (facing * 0.5 + 0.5).clamp(0.0, 1.0);
		let rgb: Vec<u8> = shell
			.iter()
			.zip(rim.iter())
			.map(|(&s, &r)| clamp8(s + (r - s) * lit))
			.collect();
		p.0 = [rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8];
	}
	out
}

/// The causeway's glyph: three pale glass planks receding up and to the
/// right — the road, three steps of it. Wardglass palette. Rng-free.
fn spell_causeway() -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	let s = SIZE as f32;
	// (centre x, centre y, half width) per plank, front to back.
	let planks = [
		(s * 0.32, s * 0.74, s * 0.26),
		(s * 0.50, s * 0.50, s * 0.21),
		(s * 0.66, s * 0.28, s * 0.16),
	];
	let half_h = s * 0.055;
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
		let mut a: f32 = 0.0;
		let mut top = 0.0;
		for (i, &(cx, cy, hw)) in planks.iter().enumerate() {
			let inside_x = (hw - (fx - cx).abs() + 0.5).clamp(0.0, 1.0);
			let local_y = fy - cy + (fx - cx) * 0.22;
			let inside_y = (half_h - local_y.abs() + 0.5).clamp(0.0, 1.0);
			let hit = inside_x * inside_y;
			if hit > a {
				a = hit;
				// The nearer planks are the brighter ones.
				top = (1.0 - i as f32 * 0.18) * if local_y > 0.8 { 0.62 } else { 1.0 };
			}
		}
		let body = [214.0 * top, 226.0 * top, 240.0 * top];
		p.0 = [
			clamp8(body[0]),
			clamp8(body[1]),
			clamp8(body[2]),
			(a * 0.86 * 255.0).round() as u8,
		];
	}
	out
}

/// A waystone monolith: a tapered standing stone with a rounded cap and
/// an incised spiral mark — the arrival pad's sign. `body` is the stone
/// colour; the port glyph is authored near-white and tinted per biome at
/// draw time, so seven ports share one texture. Rng-free.
fn monolith(body: [u8; 3]) -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	let s = SIZE as f32;
	let c = s / 2.0;
	let (top, bottom) = (s * 0.10, s * 0.94);
	let (mark_x, mark_y, mark_r) = (c, s * 0.40, s * 0.135);
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
		let t = ((fy - top) / (bottom - top)).clamp(0.0, 1.0);
		// Tapered flanks; the cap rounds off over the top tenth.
		let hw = c * (0.24 + 0.13 * t);
		let cap = ((fy - top) / (s * 0.10)).clamp(0.0, 1.0);
		let hw = hw * cap.sqrt();
		let dx = (fx - c).abs();
		let inside = (hw - dx + 0.5).clamp(0.0, 1.0);
		let tall = if fy < top {
			0.0
		} else {
			(bottom - fy + 0.5).clamp(0.0, 1.0)
		};
		let a = inside * tall;
		// The incised spiral: an Archimedean curve unwound two turns
		// around the mark's centre, darkening the stone where it passes.
		let (mx, my) = (fx - mark_x, fy - mark_y);
		let md = (mx * mx + my * my).sqrt();
		let mut groove: f32 = 0.0;
		if md < mark_r + 1.0 {
			let ang = my.atan2(mx);
			for turn in 0..2 {
				let theta = ang + std::f32::consts::TAU * turn as f32;
				let r_here =
					mark_r * (theta + std::f32::consts::TAU) / (std::f32::consts::TAU * 3.0);
				groove = groove.max((0.9 - (md - r_here).abs()).clamp(0.0, 1.0));
			}
		}
		// A light catch down the left flank keeps it from reading flat.
		let flank = ((c - fx) / hw.max(1.0)).clamp(0.0, 1.0) * 0.18;
		let rgb: Vec<u8> = body
			.iter()
			.map(|&b| clamp8(f32::from(b) * (1.0 + flank) * (1.0 - groove * 0.38)))
			.collect();
		p.0 = [rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8];
	}
	out
}

fn save(name: &str, img: RgbImage) {
	img.save(format!("{DIR}/{name}"))
		.unwrap_or_else(|e| panic!("save {name}: {e}"));
	println!("wrote {name}");
}

/// The RGBA half of `save`, for the see-through recipes (the atlas is
/// Rgba8Unorm either way; RGB PNGs decode with an opaque alpha).
fn save_rgba(name: &str, img: RgbaImage) {
	img.save(format!("{DIR}/{name}"))
		.unwrap_or_else(|e| panic!("save {name}: {e}"));
	println!("wrote {name}");
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
