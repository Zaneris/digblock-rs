//! Deterministic generator for derived block textures: palette-shift and noise
//! variations of the hand-made originals, so new blocks stay in the established
//! pixel-art style. Run manually (`cargo run -p texgen`) and commit the outputs —
//! assets are `include_bytes!`d, so generated PNGs live in git. Output matches
//! the loader contract: `SIZE`-square 8-bit RGB, non-interlaced.
//!
//! A run rewrites EVERY output, so it is not a free operation: the noisy recipes
//! below share one rng, so adding, removing or reordering a `save` rerolls the
//! noise in every output after it, and a hand-edited source whose derivative was
//! never regenerated comes back into sync the moment anyone runs this. Both are
//! fine — a reroll of the same recipe is the same texture, and re-syncing is the
//! point — but they mean the diff of a run is wider than the edit that prompted
//! it.

use digblock_core::block::BlockType as B;
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

	// Torch: the side texture maps the post's timber to the LOWER rows and
	// the flame glow to the top rows (a box face's v runs top-down from
	// the cell's top, so the shape's flame head samples the texture's TOP
	// rows); the flame top face gets its own radial-glow layer. Planed
	// lumber and not the log's bark, because a torch is a whittled stick —
	// and the DARK timber, because this texture is drawn fullbright.
	save(
		"TorchSide.png",
		torch_side(&load("LumberDark.png"), &mut rng),
	);
	save("TorchFlame.png", torch_flame(&mut rng));

	// The four ore faces and the generic tapped one. APPENDED at the end
	// deliberately: the noisy recipes share one rng, so an insertion anywhere
	// above rerolls every output after it and the diff of a regeneration would
	// be the whole texture directory.
	//
	// Each is stone with tier-coloured flecks — the gravel recipe's blob walk,
	// but coloured rather than shaded, with a light rim so a fleck reads as a
	// crystal set into rock instead of a stain on it.
	//
	// The colours are not written here. `BlockType::node_flecks` holds them,
	// because the GI material LUT casts a vein's light in its core colour and
	// the spent rocks below paint their dead flecks from the same pair: three
	// consumers, one table.
	//
	// Cores are picked BRIGHT as well as saturated — chunk.wgsl gains the ink by
	// luminance, and a near-white fleck would glow at the cost of having no
	// colour left to glow IN.
	//
	// Every tier also darkens its stone BODY hard (the `body` scale), and that
	// is the load-bearing half. Emissive is a per-BLOCK flag, not a per-texel
	// one: a glowing block renders its whole face at full albedo, so with the
	// body left undarkened a vein in a black cave reads as a glowing white rock
	// rather than as rock with glowing crystals in it. The contrast has to live
	// in the texture, because the shader has no way to express it.
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
	// Leaded glass: a pale frosted body under a dark came lattice. The
	// lattice is opaque; the panes carry a low alpha (the render's
	// Translucent bucket blends them), frost-jittered so the glazing
	// reads hand-set rather than uniform.
	save_rgba("Glass.png", glass(&mut rng));
	// Metal: a cool grey plate, planished — the flat highlights are what
	// separate it from stone at a glance.
	save("Metal.png", metal(&stone, &mut rng));
	// Crystal: an Aether-blue lattice, bright enough to carry the
	// emissive bit the way the deep ores do. The dark matrix is glass —
	// mid alpha — while the facets stay near-opaque, so what glows is
	// also what covers.
	save_rgba("Crystal.png", crystal(&mut rng));
	// The two lamps, both emissive: a metal frame around a lit pane.
	save("Lantern.png", lamp([255, 214, 138], [96, 88, 78], &mut rng));
	save(
		"Aetherlamp.png",
		lamp([186, 226, 255], [70, 74, 96], &mut rng),
	);
	// The two tool glows (SKILLS-DESIGN §7.3): what the siphon collects
	// and what the pick resonates with, each drawn as one orb and used
	// TWICE — as the emissive face of its tool's geometry, and as the
	// tool row's texture, which is where the GI material LUT reads the
	// colour of the light it casts. One artifact decides how the tool
	// looks and what its light looks like, so the two cannot drift.
	//
	// Appended after the lamps for the reason the ores were: everything
	// above keeps the bytes it had.
	save(
		"SiphonGlow.png",
		radial_glow([48, 190, 96], [12, 84, 48], &mut rng),
	);
	save(
		"PickGlow.png",
		radial_glow([60, 140, 235], [16, 40, 120], &mut rng),
	);
	// The four SPENT vein faces — what a siphon leaves standing, one per
	// tier. Appended last for the reason everything else was.
	//
	// Deliberately NOT the generic tapped stone above, and not empty
	// sockets either: this is the same rock with the same flecks in the
	// same places, drained. A worked seam should still read as the seam you
	// worked — you can tell an emberore face you have been at from a
	// duskstone one — and the block you break out of it is the only thing a
	// vein ever puts in your hands, so it had better be a trophy of that
	// tier. Telling spent from live is then a question of LIGHT rather than
	// of shape, which is exactly what a siphon takes: the live rows glow
	// and cast, and these do neither.
	for (name, block) in [
		("DepletedEmberore.png", B::Emberore),
		("DepletedDuskstone.png", B::Duskstone),
		("DepletedGlowvein.png", B::Glowvein),
		("DepletedAetherock.png", B::Aetherock),
	] {
		let (core, rim) = block.node_flecks().expect("a tier");
		save(name, spent(&stone, core, rim, &mut rng));
	}

	// The three SURFACE nodes (the essence redesign) and their spent
	// faces — the ore recipe pointed at the hosts' own bodies, so a node
	// reads as its host WITH something in it: living green in bark,
	// earthen amber in stone, spring blue in wet sand. Live faces darken
	// their bodies because they glow (the ore contrast rule); spent faces
	// stay at ordinary brightness because they are lit like any block.
	// Appended last for the reason everything above was.
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

/// Leaded glass: a frosted pale body, a dark came grid, and a bright
/// diagonal glint in each light.
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
		p.0 = [140, 205, 225, 96];
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

/// Crystal: a dark matrix shot through with bright facets. Same contrast
/// argument as the deep ores — the block is emissive, so the DARK half
/// has to be dark or the whole face glows and nothing reads.
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

/// Stone with mineral flecks: a coloured core, a lighter catch rim, and a
/// dark socket around it so each fleck sits IN the rock rather than on it.
///
/// Far fewer flecks than the gravel recipe scatters pebbles: a vein has
/// to read as rock that HAS ore in it. At the gravel count the stone
/// disappeared and the face became a solid colour, which is the same
/// mistake as a fleck with no socket, one scale up.
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

/// A tier's vein face with the light drawn out of it: the same rock, the
/// same flecks in the same places, drained to dark inclusions.
///
/// The body is left at ordinary stone brightness, and that is the one way
/// this differs structurally from [`ore`] rather than merely in palette.
/// A live vein darkens its body because it is drawn FULLBRIGHT and needs
/// the contrast to read as rock-with-crystals; a spent face is lit like
/// any other block, so the same darkening would just make it a black rock.
/// Which is the whole rule in one line: an ink texture is authored for
/// fullbright, an ordinary one is not.
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

/// Wood post darkened, with the top band of rows turned to flame glow
/// (white-hot core rows fading through yellow to orange where the flame
/// head's side faces sample).
///
/// The post is cut from the DARK timber, and that is not a style choice:
/// this texture is never lit. The block `glows`, so every pixel of it
/// goes through `emissive_ink`, whose gain rides the ink's own luminance
/// — and a pale timber has no headroom left to take that gain. It clips
/// on all three channels and the post reads as a white bar brighter than
/// the fire on top of it, inverting the one relationship the gain exists
/// to hold. Authored dark, the same weighting puts the gain where the
/// fire is, which is what its comment always claimed it did.
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

/// An orb: `core` at the centre falling to `rim` at the edge, jittered.
/// [`torch_flame`]'s shape with the hue taken as an argument.
///
/// Both callers are drawn FULLBRIGHT, so both colours are authored with
/// headroom (R29): a channel painted near white has none left to take
/// `emissive_ink`'s gain, and an orb that clips on all three is a white
/// blob rather than a coloured light. Only the dominant channel is meant
/// to reach the ceiling — which is what leaves the glow a hue at all.
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

/// Broken rock: MANY stones, not one cracked face.
///
/// The distinction is the whole job. Rubble is the only thing a natural quarry
/// hands a player now — clean rock is bench work — so the two faces are a
/// decision the player reads off a wall. Stone's own body with hairline fracture
/// walks over it cannot carry that: at any distance it is smooth stone with
/// scratches on it, the same value and the same grain.
///
/// So the SILHOUETTE changes rather than the detail. A wrapping Worley partition
/// cuts the tile into a dozen-odd cobbles, each taking its own value offset — a
/// face of stones at visibly different tones is the cue that survives being
/// shrunk to an inventory icon, where every crack disappears — and the band
/// where two sites run close darkens into the gap between them, with the body
/// noise folded into the threshold so the gap chips rather than curving cleanly.
/// Everything wraps modularly, so the tile still tiles.
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
