//! Deterministic generator for derived block textures: palette-shift and noise
//! variations of the hand-made originals, so new blocks stay in the established
//! pixel-art style. Run manually (`cargo run -p texgen`) and commit the outputs —
//! assets are `include_bytes!`d, so generated PNGs live in git. Output matches
//! the loader contract: `SIZE`-square 8-bit, non-interlaced — RGB, or RGBA for
//! the rows whose alpha the Translucent bucket reads (`save_rgba`).
//!
//! A run rewrites EVERY output, so it is not a free operation: the noisy recipes
//! below share one rng, so adding, removing or reordering a `save` rerolls the
//! noise in every output after it, and a hand-edited source whose derivative was
//! never regenerated comes back into sync the moment anyone runs this. Both are
//! fine — a reroll of the same recipe is the same texture, and re-syncing is the
//! point — but they mean the diff of a run is wider than the edit that prompted
//! it.

use digblock_core::block::{BlockType as B, Essence};
use image::{RgbImage, RgbaImage};

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

	// Broken rock — the ONLY thing a natural quarry hands you now, since
	// clean rock is bench work (SKILLS-DESIGN §7.1). It has to read as
	// many stones rather than as one cracked face, which is a question of
	// silhouette and not of detail; see the recipe.
	save("Rubble.png", rubble(&stone, &mut rng));

	// Torch: the side texture maps the post's timber to the LOWER rows and the flame glow to the
	// top rows (a box face's v runs top-down from the cell's top, so the shape's flame head samples
	// the texture's TOP rows); the flame top face gets its own radial-glow layer. Planed lumber
	// rather than the log's bark, because a torch is a whittled stick — and the DARK timber,
	// because this texture is drawn fullbright.
	save(
		"TorchSide.png",
		torch_side(&load("LumberDark.png"), &mut rng),
	);
	save("TorchFlame.png", torch_flame(&mut rng));

	// The four ore faces and the generic tapped one, APPENDED deliberately: the noisy recipes share
	// one rng, so an insertion above rerolls every output after it. The colours live in
	// `BlockType::node_flecks`, not here — the GI LUT and the spent rocks read the same pair. Cores
	// are BRIGHT as well as saturated (chunk.wgsl gains ink by luminance), and every tier darkens
	// its stone BODY, since emissive is a per-BLOCK flag and the contrast must live in the texture.
	for (name, block, body) in [
		("Emberore.png", B::Emberore, 0.34),
		("Duskstone.png", B::Duskstone, 0.32),
		("Glowvein.png", B::Glowvein, 0.30),
		("Aetherock.png", B::Aetherock, 0.30),
	] {
		let (core, rim) = block.node_flecks().expect("a tier");
		save(name, ore(&stone, core, rim, body, &mut rng));
	}
	// A tapped vein face: the flecks are GONE and their sockets are not.
	// Empty pockets are what makes it read as spent rather than as one more
	// grey rock — the miner's feedback that this face is done. Superseded
	// by the per-tier spent rocks at the end of this run and kept only
	// because cells written before them still name it.
	save("DepletedStone.png", depleted(&stone, &mut rng));

	// The dressed stones (SKILLS-DESIGN §7.2's later materials): the same
	// rock, worked. Bricks get a running bond, polished stone gets its
	// grain smoothed out and a faint sheen.
	save("StoneBrick.png", bricks(&stone, 16, 8, &mut rng));
	save(
		"SandstoneBrick.png",
		bricks(&load("Sandstone.png"), 16, 8, &mut rng),
	);
	save("PolishedStone.png", polished(&stone, &mut rng));
	// Clear glass: one flat near-white tint at a modest alpha, so most of what
	// you see through a pane is the world behind it (the render's
	// Translucent bucket multiplies the backdrop by its transmittance).
	save_rgba("Glass.png", glass(&mut rng));
	// Metal: a cool grey plate, planished — the flat highlights are what
	// separate it from stone at a glance.
	save("Metal.png", metal(&stone, &mut rng));
	// Crystal: an Aether-blue body carrying the emissive bit the way the
	// deep ores do, with fine cracks a shade brighter. Near-opaque
	// throughout — it is see-through, but it is a solid, not a window.
	save_rgba("Crystal.png", crystal(&mut rng));
	// The two lamps, both emissive: a metal frame around a lit pane.
	save("Lantern.png", lamp([255, 214, 138], [96, 88, 78], &mut rng));
	save(
		"Aetherlamp.png",
		lamp([186, 226, 255], [70, 74, 96], &mut rng),
	);
	// The two tool glows (SKILLS-DESIGN §7.3): what the siphon collects and what the pick resonates
	// with, each drawn as one orb and used TWICE — as the emissive face of its tool's geometry, and
	// as the tool row's texture, where the GI material LUT reads the colour of the light it casts.
	// One artifact decides how the tool looks and what its light looks like, so the two cannot
	// drift. Appended after the lamps so everything above keeps the bytes it had.
	save(
		"SiphonGlow.png",
		radial_glow([48, 190, 96], [12, 84, 48], &mut rng),
	);
	save(
		"PickGlow.png",
		radial_glow([60, 140, 235], [16, 40, 120], &mut rng),
	);
	// The four SPENT vein faces — what a siphon leaves standing, one per tier. Appended last for
	// the reason everything else was. Deliberately NOT the generic tapped stone above, and not
	// empty sockets: this is the same rock with the same flecks in the same places, drained, so a
	// worked seam still reads as the seam you worked. Telling spent from live is then a question of
	// LIGHT — the live rows glow and cast, and these do neither.
	for (name, block) in [
		("DepletedEmberore.png", B::Emberore),
		("DepletedDuskstone.png", B::Duskstone),
		("DepletedGlowvein.png", B::Glowvein),
		("DepletedAetherock.png", B::Aetherock),
	] {
		let (core, rim) = block.node_flecks().expect("a tier");
		save(name, spent(&stone, core, rim, &mut rng));
	}

	// The three SURFACE nodes (the essence redesign) and their spent faces — the ore recipe
	// pointed at the hosts' own bodies, so a node reads as its host WITH something in it: living
	// green in bark, earthen amber in stone, spring blue in wet sand. Live faces darken their
	// bodies because they glow (the ore contrast rule); spent faces stay at ordinary brightness,
	// being lit like any block. Appended last for the reason everything above was.
	let log = load("Log.png");
	{
		let (core, rim) = B::Heartwood.node_flecks().expect("a node");
		save("Heartwood.png", ore(&log, core, rim, 0.42, &mut rng));
		save("DrainedHeartwood.png", spent(&log, core, rim, &mut rng));
	}
	{
		let (core, rim) = B::Leystone.node_flecks().expect("a node");
		save("Leystone.png", ore(&stone, core, rim, 0.34, &mut rng));
		save("SpentLeystone.png", spent(&stone, core, rim, &mut rng));
	}
	{
		let (core, rim) = B::Springstone.node_flecks().expect("a node");
		save("Springstone.png", ore(&sand, core, rim, 0.42, &mut rng));
		save("SpentSpringstone.png", spent(&sand, core, rim, &mut rng));
	}
	// The tinted panes — clean glass in a colour, drawn with NO rng at
	// all and saved after everything else, so adding one can never
	// reroll an earlier recipe.
	for (name, tint) in [
		("GlassRed.png", [225u8, 58, 48]),
		("GlassGreen.png", [84u8, 205, 96]),
		("GlassBlue.png", [72u8, 128, 232]),
		("GlassAmber.png", [235u8, 172, 64]),
	] {
		save_rgba(name, tinted_pane(tint));
	}
	// Wardglass — the conjured pane: whiter and denser than clear glass
	// so a road reads as glass from above and below. Rng-free, same rule.
	save_rgba("Wardglass.png", wardglass());
	// The essence chips — the UI's only sprites, one flake per essence in
	// its own palette. Also rng-free and also saved last, for the same
	// reason as the panes.
	for (name, e) in [
		("EssenceVitae.png", Essence::Vitae),
		("EssenceTerra.png", Essence::Terra),
		("EssenceUnda.png", Essence::Unda),
		("EssenceFerrum.png", Essence::Ferrum),
		("EssenceAether.png", Essence::Aether),
	] {
		let (core, rim) = e.palette();
		save_rgba(name, essence_flake(core, rim));
	}
	// The spell glyphs — the spell bar's sprites, one per castable shape
	// plus the two waystone marks (the port one authored near-white as a
	// tint canvas, coloured per biome at draw time). Rng-free and saved
	// last, the essence-chip rule.
	save_rgba("SpellWisp.png", spell_wisp());
	save_rgba("SpellKindle.png", spell_kindle());
	save_rgba("SpellSphere.png", spell_sphere());
	save_rgba("SpellCauseway.png", spell_causeway());
	save_rgba("SpellHome.png", monolith([150, 150, 158]));
	save_rgba("SpellPort.png", monolith([236, 239, 244]));
}

/// A running-bond course: `h`-tall rows offset by half a brick each
/// course, with a darkened mortar line and a light catch on the top edge
/// of every brick.
fn bricks(base: &RgbImage, w: u32, h: u32, rng: &mut Rng) -> RgbImage {
	let mut out = base.clone();
	for (x, y, p) in out.enumerate_pixels_mut() {
		let course = y / h;
		let offset = if course.is_multiple_of(2) { 0 } else { w / 2 };
		let bx = (x + offset) % w;
		let by = y % h;
		let n = rng.signed(5);
		let d = if by == 0 || bx == 0 {
			// Mortar.
			-46
		} else if by == 1 {
			// The light catch under the joint above.
			14
		} else {
			0
		};
		for c in p.0.iter_mut() {
			*c = add8(*c, d + n);
		}
	}
	out
}

/// Stone with its grain averaged away and a soft diagonal sheen — the
/// difference between quarried and dressed is mostly the absence of
/// texture, so this REMOVES rather than adds.
fn polished(stone: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = stone.clone();
	let src = stone.clone();
	let s = SIZE as i32;
	for (x, y, p) in out.enumerate_pixels_mut() {
		let mut acc = [0i32; 3];
		for dy in -1..=1 {
			for dx in -1..=1 {
				let q = src.get_pixel(
					(x as i32 + dx).rem_euclid(s) as u32,
					(y as i32 + dy).rem_euclid(s) as u32,
				);
				for (a, v) in acc.iter_mut().zip(q.0.iter()) {
					*a += i32::from(*v);
				}
			}
		}
		// A gentle diagonal band, and the last of the grain.
		let sheen = (((x + y) % SIZE) as f32 / SIZE as f32 * std::f32::consts::TAU).sin() * 7.0;
		let n = rng.signed(2);
		for (out, a) in p.0.iter_mut().zip(acc.iter()) {
			*out = clamp8(*a as f32 / 9.0 + sheen + n as f32);
		}
	}
	out
}

/// Clear glass: one flat pale tint at one flat alpha. No came grid and
/// no frost — a pane reads by what it does to the world behind it, and a
/// painted frame only fights the transmission. The rng draws are kept
/// and burnt unread so every later recipe's noise is unchanged.
fn glass(rng: &mut Rng) -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	let cell = SIZE / 4;
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (cx, cy) = (x % cell, y % cell);
		let frost = rng.signed(6);
		let glint = if cx + cy > cell + cell / 3 && cx + cy < cell + cell / 2 {
			26
		} else {
			0
		};
		// A clean tinted pane: one flat colour, one flat alpha, no came
		// lattice — glass reads by what it does to the world behind it,
		// not by a painted frame. The frost draw above is burnt unread
		// so the rng stream (and every later recipe) is unchanged.
		let _ = (frost, glint, cx, cy);
		p.0 = [210, 228, 236, 120];
	}
	out
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

/// An essence flake: a disc in the core colour, lit from the upper left — a rim-coloured crescent
/// and a small highlight there, the far edge darkened — over a transparent field. The field's RGB
/// is the core colour rather than black: the loader mips the array, and a chip drawn at glyph size
/// samples a mip where transparent black would bleed a dark fringe into the disc's edge. Rng-free,
/// so it never rerolls a neighbour.
fn essence_flake(core: [u8; 3], rim: [u8; 3]) -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	let c = SIZE as f32 / 2.0;
	let radius = c * 0.84;
	// The light's direction (upper left) and the highlight's centre.
	let light = (-0.6f32, -0.8f32);
	let (hx, hy) = (c + light.0 * radius * 0.45, c + light.1 * radius * 0.45);
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
		let d = (dx * dx + dy * dy).sqrt();
		// One-pixel analytic falloff at the edge.
		let alpha = (radius + 0.5 - d).clamp(0.0, 1.0);
		// Lit side toward the light: +1 facing it, -1 away.
		let facing = if d > 0.0 {
			(dx * light.0 + dy * light.1) / d
		} else {
			0.0
		};
		let edge = (d / radius).clamp(0.0, 1.0);
		// The crescent: rim colour where the edge faces the light; the
		// far edge darkens toward a third of the core.
		let toward = (facing * edge).max(0.0).powf(1.5);
		let away = ((-facing) * edge).max(0.0).powf(2.0);
		// The highlight: a soft blob near the light.
		let hd = ((x as f32 + 0.5 - hx).powi(2) + (y as f32 + 0.5 - hy).powi(2)).sqrt();
		let gloss = (1.0 - hd / (radius * 0.35)).clamp(0.0, 1.0).powi(2);
		let mut rgb = [0u8; 3];
		for (i, v) in rgb.iter_mut().enumerate() {
			let base = f32::from(core[i]);
			let lit = base + (f32::from(rim[i]) - base) * (toward * 0.8 + gloss).min(1.0);
			*v = clamp8(lit * (1.0 - away * 0.55));
		}
		p.0 = [rgb[0], rgb[1], rgb[2], (alpha * 255.0).round() as u8];
	}
	out
}

/// Planished metal plate: stone's noise flattened, cooled and lifted,
/// with a few hammer highlights.
fn metal(stone: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = stone.clone();
	for p in out.pixels_mut() {
		let avg = (i32::from(p.0[0]) + i32::from(p.0[1]) + i32::from(p.0[2])) / 3;
		let flat = (avg - 128) / 3 + 150;
		let n = rng.signed(4);
		p.0 = [
			clamp8((flat + n) as f32 * 0.96),
			clamp8((flat + n) as f32 * 0.98),
			clamp8((flat + n) as f32 * 1.06),
		];
	}
	let s = SIZE as i32;
	for _ in 0..14 {
		let (cx, cy) = (rng.range(SIZE as u64) as i32, rng.range(SIZE as u64) as i32);
		for dy in 0..3 {
			for dx in 0..3 {
				let p = out.get_pixel_mut(
					(cx + dx).rem_euclid(s) as u32,
					(cy + dy).rem_euclid(s) as u32,
				);
				let d = if dx + dy < 2 { 22 } else { -14 };
				for c in p.0.iter_mut() {
					*c = add8(*c, d);
				}
			}
		}
	}
	out
}

/// Crystal: a near-uniform translucent blue body under fine cracks. The
/// facet lattice is retired — the block reads by its glow and what it
/// passes, so painted structure only competed with both. Its jitter draw
/// is still burnt at the old band positions to hold the rng stream.
fn crystal(rng: &mut Rng) -> RgbaImage {
	let mut out = RgbaImage::new(SIZE, SIZE);
	for (x, y, p) in out.enumerate_pixels_mut() {
		let n = rng.signed(5);
		// A plain gem: a near-uniform translucent blue body — the glow
		// comes from the ink gain, not from painted structure.
		p.0 = [
			add8(58, n * 2),
			add8(78, n * 2),
			add8(114, n * 2),
			add8(214, n * 2),
		];
		// The retired facet lattice still BURNS its jitter draw at the
		// old band positions, so the rng stream (and every later
		// recipe's output) is unchanged.
		let a = (x + y) % 19;
		let b = (x + SIZE - y) % 23;
		if a < 3 || b < 2 {
			let _ = rng.signed(14);
		}
		// Small cracks: a few thin wandering lines — a staircase wobble
		// on a coarse period, long enough to read as a crack rather
		// than mipping down to glitter. Position-derived, so they cost
		// no draws and tile.
		let wob_v = (y / 3 + y / 7) % 5;
		let crack_v = (x + wob_v) % 21 == 0 && y % 32 < 22;
		let wob_h = (x / 4 + x / 9) % 4;
		let crack_h = (y + wob_h) % 26 == 0 && (x + 11) % 32 < 20;
		if crack_v || crack_h {
			p.0 = [add8(118, n * 3), add8(162, n * 3), add8(204, n * 3), 236];
		}
	}
	out
}

/// A lamp face: a lit pane inside a metal frame. `lit` is the glass,
/// `frame` the housing.
fn lamp(lit: [u8; 3], frame: [u8; 3], rng: &mut Rng) -> RgbImage {
	let mut out = RgbImage::new(SIZE, SIZE);
	let edge = SIZE / 8;
	let c = SIZE as f32 / 2.0;
	for (x, y, p) in out.enumerate_pixels_mut() {
		let n = rng.signed(6);
		if x < edge || y < edge || x >= SIZE - edge || y >= SIZE - edge {
			p.0 = [add8(frame[0], n), add8(frame[1], n), add8(frame[2], n)];
			continue;
		}
		// Brightest at the middle, so the pane reads as a source rather
		// than a flat colour.
		let d = (((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c).min(1.0);
		let k = 1.0 - 0.45 * d;
		p.0 = [
			clamp8(f32::from(lit[0]) * k + n as f32),
			clamp8(f32::from(lit[1]) * k + n as f32),
			clamp8(f32::from(lit[2]) * k + n as f32),
		];
	}
	out
}

/// Stone with mineral flecks: a coloured core, a lighter catch rim, and a dark socket around it so
/// each fleck sits IN the rock rather than on it. Far fewer flecks than the gravel recipe scatters
/// pebbles — a vein has to read as rock that HAS ore in it; at the gravel count the stone
/// disappears and the face becomes a solid colour, the same mistake as a fleck with no socket, one
/// scale up.
fn ore(stone: &RgbImage, core: [u8; 3], rim: [u8; 3], body: f32, rng: &mut Rng) -> RgbImage {
	let mut out = stone.clone();
	for p in out.pixels_mut() {
		let n = rng.signed(6);
		p.0 = [
			clamp8(f32::from(add8(p.0[0], n)) * body),
			clamp8(f32::from(add8(p.0[1], n)) * body),
			clamp8(f32::from(add8(p.0[2], n)) * body),
		];
	}
	let s = SIZE as i32;
	for _ in 0..18 {
		let (cx, cy) = (rng.range(SIZE as u64) as i32, rng.range(SIZE as u64) as i32);
		let r = 2 + rng.range(2) as i32;
		for dy in -r - 1..=r + 1 {
			for dx in -r - 1..=r + 1 {
				let d2 = dx * dx + dy * dy;
				let p = out.get_pixel_mut(
					(cx + dx).rem_euclid(s) as u32,
					(cy + dy).rem_euclid(s) as u32,
				);
				if d2 <= (r - 1).max(0) * (r - 1).max(0) {
					let j = rng.signed(10);
					p.0 = [add8(core[0], j), add8(core[1], j), add8(core[2], j)];
				} else if d2 <= r * r {
					let j = rng.signed(8);
					p.0 = [add8(rim[0], j), add8(rim[1], j), add8(rim[2], j)];
				} else if d2 <= (r + 1) * (r + 1) {
					p.0 = [add8(p.0[0], -30), add8(p.0[1], -30), add8(p.0[2], -30)];
				}
			}
		}
	}
	out
}

/// Stone with the flecks knocked out: empty sockets, darkest at the
/// centre, with a light lip where the pick broke the rim.
fn depleted(stone: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = stone.clone();
	for p in out.pixels_mut() {
		let n = rng.signed(6);
		p.0 = [add8(p.0[0], n), add8(p.0[1], n), add8(p.0[2], n)];
	}
	let s = SIZE as i32;
	for _ in 0..18 {
		let (cx, cy) = (rng.range(SIZE as u64) as i32, rng.range(SIZE as u64) as i32);
		let r = 2 + rng.range(2) as i32;
		for dy in -r - 1..=r + 1 {
			for dx in -r - 1..=r + 1 {
				let d2 = dx * dx + dy * dy;
				let p = out.get_pixel_mut(
					(cx + dx).rem_euclid(s) as u32,
					(cy + dy).rem_euclid(s) as u32,
				);
				if d2 <= (r - 1).max(0) * (r - 1).max(0) {
					let j = rng.signed(5);
					p.0 = [
						add8(p.0[0], -58 + j),
						add8(p.0[1], -58 + j),
						add8(p.0[2], -56 + j),
					];
				} else if d2 <= r * r {
					p.0 = [add8(p.0[0], -34), add8(p.0[1], -34), add8(p.0[2], -32)];
				} else if d2 <= (r + 1) * (r + 1) {
					p.0 = [add8(p.0[0], 14), add8(p.0[1], 14), add8(p.0[2], 14)];
				}
			}
		}
	}
	out
}

/// A tier's vein face with the light drawn out of it: the same rock, the same flecks in the same
/// places, drained to dark inclusions. The body is left at ordinary stone brightness, the one way
/// this differs structurally from [`ore`] rather than merely in palette — a live vein darkens its
/// body because it is drawn FULLBRIGHT, while a spent face is lit like any block and the same
/// darkening would make it a black rock. An ink texture is authored for fullbright; this is not.
fn spent(stone: &RgbImage, core: [u8; 3], rim: [u8; 3], rng: &mut Rng) -> RgbImage {
	// How much of the tier's colour survives the draw. The rim keeps more
	// than the core so a drained fleck still reads as something set INTO
	// the rock rather than a smudge on it — the same job the light catch
	// does on a live one, at a fraction of the brightness.
	const CORE_LEFT: f32 = 0.34;
	const RIM_LEFT: f32 = 0.42;
	let drain = |c: [u8; 3], k: f32| {
		[
			clamp8(f32::from(c[0]) * k),
			clamp8(f32::from(c[1]) * k),
			clamp8(f32::from(c[2]) * k),
		]
	};
	ore(
		stone,
		drain(core, CORE_LEFT),
		drain(rim, RIM_LEFT),
		1.0,
		rng,
	)
}

/// Wood post darkened, with the top band of rows turned to flame glow (white-hot core rows fading
/// through yellow to orange where the flame head's side faces sample). The post is cut from the
/// DARK timber, and that is not a style choice: this texture is never lit, so every pixel goes
/// through `emissive_ink`, whose gain rides the ink's own luminance — a pale timber clips on all
/// three channels and reads as a white bar brighter than the fire on top of it.
fn torch_side(wood: &RgbImage, rng: &mut Rng) -> RgbImage {
	let mut out = wood.clone();
	let flame_rows = SIZE * 5 / 12; // the flame band, in the shape's twelfths
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

/// An orb: `core` at the centre falling to `rim` at the edge, jittered — [`torch_flame`]'s shape
/// with the hue taken as an argument. Both callers are drawn FULLBRIGHT, so both colours are
/// authored with headroom (R29): a channel painted near white has none left to take
/// `emissive_ink`'s gain, and an orb clipping on all three is a white blob, not a coloured light.
/// Only the dominant channel is meant to reach the ceiling — which is what leaves the glow a hue.
fn radial_glow(core: [u8; 3], rim: [u8; 3], rng: &mut Rng) -> RgbImage {
	let mut out = RgbImage::new(SIZE, SIZE);
	let c = SIZE as f32 / 2.0;
	for (x, y, p) in out.enumerate_pixels_mut() {
		let d = (((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c).min(1.0);
		let j = rng.signed(8) as f32;
		for (v, (&a, &b)) in p.0.iter_mut().zip(core.iter().zip(rim.iter())) {
			*v = clamp8(f32::from(a) + (f32::from(b) - f32::from(a)) * d + j);
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

/// Broken rock: MANY stones, not one cracked face. Rubble is the only thing a natural quarry hands
/// a player now — clean rock is bench work — so the two faces are a decision read off a wall,
/// which stone's body with fracture walks over it cannot carry: at any distance that is smooth
/// stone with scratches. So the SILHOUETTE changes, not the detail: a wrapping Worley partition
/// cuts the tile into cobbles at their own tones, the one cue that survives an inventory icon.
fn rubble(stone: &RgbImage, rng: &mut Rng) -> RgbImage {
	let s = SIZE as i32;
	// Sites on a jittered lattice: chunky stones rather than a fine
	// crazing, and no cell large enough to read as a flat face of its own.
	const CELLS: i32 = 4;
	let step = s / CELLS;
	let sites: Vec<(i32, i32, i32)> = (0..CELLS * CELLS)
		.map(|i| {
			let (gx, gy) = (i % CELLS, i / CELLS);
			(
				gx * step + rng.range(step as u64) as i32,
				gy * step + rng.range(step as u64) as i32,
				// Each stone's own tone. Biased dark: broken rock is dirtier
				// than the face it came off.
				rng.signed(20) - 8,
			)
		})
		.collect();
	// Wrapped separation on one axis — the near copy of the tile is what
	// keeps a site on the left edge a neighbour of one on the right.
	let wrap = |d: i32| {
		let d = d.rem_euclid(s);
		d.min(s - d)
	};
	let mut out = stone.clone();
	for (x, y, p) in out.enumerate_pixels_mut() {
		let (x, y) = (x as i32, y as i32);
		// The two nearest sites: the closest says which stone this is, and
		// how close the second one runs says whether we are in the gap.
		let (mut first, mut second) = (i32::MAX, i32::MAX);
		let mut tone = 0;
		for &(sx, sy, t) in &sites {
			let (dx, dy) = (wrap(x - sx), wrap(y - sy));
			let d2 = dx * dx + dy * dy;
			if d2 < first {
				second = first;
				first = d2;
				tone = t;
			} else if d2 < second {
				second = d2;
			}
		}
		// Grubby body noise, and the same draw ragged the gap's edge — one
		// jitter doing both is what keeps a chipped edge and a chipped
		// surface the same size of chip.
		let n = rng.signed(7) - 3;
		let edge = (second as f32).sqrt() - (first as f32).sqrt() + n as f32 * 0.12;
		let d = if edge < 1.35 {
			// The mortar gap between two stones.
			-52 + n
		} else {
			tone + n
		};
		p.0 = [add8(p.0[0], d), add8(p.0[1], d), add8(p.0[2], d)];
	}
	// The catch light: one pass over the finished field, lifting a pixel
	// whose up-left neighbour sits in a gap. Read from a copy, so a lit
	// pixel cannot go on to light the next one into a smear.
	let lit = out.clone();
	for (x, y, p) in out.enumerate_pixels_mut() {
		let up = lit.get_pixel(
			(x as i32 - 1).rem_euclid(s) as u32,
			(y as i32 - 1).rem_euclid(s) as u32,
		);
		let here = lit.get_pixel(x, y);
		if i32::from(here.0[0]) - i32::from(up.0[0]) > 24 {
			p.0 = [add8(p.0[0], 14), add8(p.0[1], 14), add8(p.0[2], 14)];
		}
	}
	// Chipped corners on top of the cobbles — the grit that reads at
	// arm's length, where the partition is reading at room's length.
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
			let inside_y = (half_h - (fy - cy).abs() + 0.5).clamp(0.0, 1.0);
			let hit = inside_x * inside_y;
			if hit > a {
				a = hit;
				// The nearer planks are the brighter ones.
				top = 1.0 - i as f32 * 0.18;
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

/// Green → yellow-tan: hue swung back toward yellow, a touch desaturated
/// and a touch lighter.
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
	// Horizontal strata: darken bands of a few rows by a per-band amount,
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
	// Pebbles: small lightened/darkened blobs with a darker rim one pixel
	// wide, wrapped modularly so the texture keeps tiling.
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
