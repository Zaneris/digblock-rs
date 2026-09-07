//! Tileable material recipes. Broad forms carry the material at a distance;
//! quantized colour clusters and small chips provide the close-up pixel detail.
//! Each surface owns its seed, and a deposit's live/spent pair shares one mask.

use super::{RgbImage, RgbaImage, Rng, SIZE, add8, clamp8};

fn hash(x: i32, y: i32, seed: u64) -> f32 {
	let mut rng =
		Rng(seed ^ (x as u64).wrapping_mul(0x9e37_79b9) ^ (y as u64).wrapping_mul(0x85eb_ca77));
	rng.range(65536) as f32 / 65535.0
}

fn noise(x: f32, y: f32, step: i32, seed: u64) -> f32 {
	let count = SIZE as i32 / step;
	let (x, y) = (x / step as f32, y / step as f32);
	let (ix, iy) = (x.floor() as i32, y.floor() as i32);
	let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
	let (fx, fy) = (smooth(x - x.floor()), smooth(y - y.floor()));
	let h = |dx: i32, dy: i32| {
		hash(
			(ix + dx).rem_euclid(count),
			(iy + dy).rem_euclid(count),
			seed,
		)
	};
	let a = h(0, 0) * (1.0 - fx) + h(1, 0) * fx;
	let b = h(0, 1) * (1.0 - fx) + h(1, 1) * fx;
	(a * (1.0 - fy) + b * fy) * 2.0 - 1.0
}

fn tone(base: [u8; 3], value: f32, warmth: f32) -> [u8; 3] {
	[
		clamp8(f32::from(base[0]) + value + warmth),
		clamp8(f32::from(base[1]) + value),
		clamp8(f32::from(base[2]) + value - warmth),
	]
}

fn band(v: f32, step: f32) -> f32 {
	(v / step).round() * step
}

fn field(mut f: impl FnMut(f32, f32) -> [u8; 3]) -> RgbImage {
	RgbImage::from_fn(SIZE, SIZE, |x, y| image::Rgb(f(x as f32, y as f32)))
}

pub(super) fn dirt() -> RgbImage {
	field(|x, y| {
		let clod = noise(x, y, 8, 141);
		let fine = noise(x, y, 2, 142);
		let split = (clod + fine * 0.15).abs();
		let crevice = if split < 0.055 { -9.0 } else { 0.0 };
		let grit = if hash(x as i32 / 2, y as i32 / 2, 143) > 0.976 {
			if (x as i32 + y as i32) % 2 == 0 {
				7.0
			} else {
				-5.0
			}
		} else {
			0.0
		};
		tone(
			[123, 91, 65],
			band(clod * 13.0 + noise(x, y, 16, 144) * 7.0 + fine * 3.0, 3.0) + crevice + grit,
			clod * 2.0,
		)
	})
}

/// Green and dry turf share their tuft geometry; only the grass palette changes.
pub(super) fn grass_top(dry: bool) -> RgbImage {
	field(|x, y| {
		let patch = noise(x, y, 8, 151);
		let tuft = noise(x, y, 2, 152);
		let gx = x as i32 / 4;
		let gy = y as i32 / 4;
		let blade_x = (hash(gx, gy, 153) * 3.0) as i32;
		let blade = (x as i32 % 4 == blade_x) && y as i32 % 4 < 2;
		let catch = if blade { 6.0 } else { 0.0 };
		tone(
			if dry { [151, 139, 67] } else { [80, 122, 43] },
			band(patch * 11.0 + tuft * 7.0, 3.0) + catch,
			patch * 1.5,
		)
	})
}

/// The exposed soil is the dirt recipe itself. A ragged, rooted turf edge replaces the sawtooth.
pub(super) fn grass_side(dirt: &RgbImage, top: &RgbImage) -> RgbImage {
	field(|x, y| {
		let fringe = (12.0 + noise(x, 0.0, 8, 154) * 5.0 + noise(x, 0.0, 2, 155) * 2.0).round();
		let soil = dirt.get_pixel(x as u32, y as u32).0;
		if y < fringe {
			let turf = top.get_pixel(x as u32, y as u32).0;
			let lip = if y >= fringe - 2.0 { -12.0 } else { 0.0 };
			tone(turf, lip, 0.0)
		} else {
			let root =
				x as i32 % 5 == (hash(x as i32 / 5, 0, 156) * 4.0) as i32 && y < fringe + 3.0;
			tone(
				soil,
				if root {
					7.0
				} else if y < fringe + 1.0 {
					-9.0
				} else {
					0.0
				},
				0.0,
			)
		}
	})
}

/// Elongated, irregular bark plates with narrow shaded fissures and restrained fine grain.
pub(super) fn bark() -> RgbImage {
	let plates: Vec<Cell> = (0..16)
		.map(|i| Cell {
			x: (i % 8) as f32 * 8.0 + 1.0 + hash(i, 0, 161) * 6.0,
			y: (i / 8) as f32 * 32.0 + 3.0 + hash(i, 1, 161) * 24.0,
			tone: hash(i, 2, 161) * 2.0 - 1.0,
		})
		.collect();
	field(|x, y| {
		let wx = x + noise(x, y, 8, 162) * 1.6 + noise(x, y, 2, 163) * 0.35;
		let wy = y + noise(x, y, 8, 164) * 2.0;
		let (mut first, mut second, mut index, mut neighbour) = (f32::MAX, f32::MAX, 0, 0);
		for (i, c) in plates.iter().enumerate() {
			let d = (wrap(wx - c.x).powi(2) + (wrap(wy - c.y) * 0.28).powi(2)).sqrt();
			if d < first {
				second = first;
				neighbour = index;
				first = d;
				index = i;
			} else if d < second {
				second = d;
				neighbour = i;
			}
		}
		let c = &plates[index];
		let edge = second - first;
		let side = &plates[neighbour];
		let facing = wrap(c.x - side.x) + wrap(c.y - side.y) * 0.16;
		let split = if edge < 0.48 {
			-28.0
		} else if edge < 0.95 {
			-13.0
		} else if edge < 1.65 && facing > 0.0 {
			9.0
		} else {
			0.0
		};
		let relief = (-wrap(wx - c.x) * 1.0 - wrap(wy - c.y) * 0.10).clamp(-7.0, 7.0);
		let crosswise = (wrap(c.y - side.y) * 0.28).abs() > wrap(c.x - side.x).abs();
		let split = split * if crosswise { 0.55 } else { 1.0 };
		let grain = noise(x * 4.0, y, 16, 167);
		let hairline = if grain.abs() < 0.045 && noise(x, y, 8, 168) > 0.1 {
			-7.0
		} else {
			0.0
		};
		tone(
			[113, 91, 74],
			split
				+ c.tone * 9.0
				+ band(relief + grain * 7.0 + noise(x, y, 2, 169) * 3.0, 2.0)
				+ hairline,
			c.tone * 2.0,
		)
	})
}

/// End grain is a bounded cut face: eccentric growth rings inside a narrow bark rim.
pub(super) fn log_top(bark: &RgbImage) -> RgbImage {
	field(|x, y| {
		let edge = x.min(63.0 - x).min(y.min(63.0 - y));
		if edge < 2.0 {
			return bark.get_pixel(x as u32, y as u32).0;
		}
		let (dx, dy) = (x - 29.0, y - 33.0);
		let radius = (dx * dx * 0.94 + dy * dy).sqrt();
		let grain = radius + noise(x, y, 8, 165) * 0.8;
		let ring = grain.rem_euclid(5.5);
		let shade = if ring < 0.9 {
			-17.0
		} else if ring < 1.8 {
			6.0
		} else {
			0.0
		};
		let angle = dy.atan2(dx);
		let crack = radius > 13.0
			&& ((angle - 0.7).abs() * radius < 0.6 || (angle + 2.3).abs() * radius < 0.5);
		tone(
			[173, 132, 82],
			shade + band(noise(x, y, 4, 166) * 4.0, 2.0) + if crack { -23.0 } else { 0.0 },
			0.0,
		)
	})
}

/// The lumber colours share planed grain and small knots, with quieter relief than bark.
pub(super) fn lumber(base: [u8; 3]) -> RgbImage {
	field(|x, y| {
		let dx = wrap(x - 13.0);
		let dy = wrap(y - 39.0);
		let knot = (dx * dx / 10.0 + dy * dy / 54.0).sqrt();
		let curl = if knot < 3.0 {
			(3.0 - knot) * dx.signum() * 1.2
		} else {
			0.0
		};
		let drift = noise(x, y, 32, 171) * 2.0 + curl;
		let grain = ((x + drift) * std::f32::consts::TAU / 7.0).sin();
		let fine = noise(x * 4.0, y, 16, 172);
		let shade = if knot < 0.6 {
			-19.0
		} else if grain < -0.82 {
			-8.0
		} else {
			band(grain * 3.0, 2.0)
		};
		let joint = match x as i32 % 32 {
			0 => -14.0,
			1 => 5.0,
			31 => -5.0,
			_ => 0.0,
		};
		let board = if x < 32.0 { 2.0 } else { -2.0 };
		tone(base, shade + joint + board + band(fine * 3.0, 2.0), fine)
	})
}

pub(super) fn stone() -> RgbImage {
	field(|x, y| {
		let mass = noise(x, y, 16, 11);
		let grain = noise(x, y, 4, 12);
		let seam = (noise(x, y, 32, 13) + grain * 0.12).abs();
		let fracture = if seam < 0.022 {
			-13.0
		} else if seam < 0.05 {
			4.0
		} else {
			0.0
		};
		tone(
			[127, 128, 132],
			band(mass * 17.0 + grain * 5.0, 3.0) + fracture,
			noise(x, y, 16, 14) * 3.0,
		)
	})
}

pub(super) fn sand(solid: bool) -> RgbImage {
	field(|x, y| {
		let wave = (y * std::f32::consts::TAU / 16.0 + noise(x, y, 32, 22) * 1.6).sin();
		let detail = noise(x, y, 2, 23);
		let (base, value) = if solid {
			(
				[190, 165, 112],
				band(wave * 7.0 + noise(x, y, 8, 24) * 8.0, 3.0),
			)
		} else {
			(
				[222, 205, 152],
				band(wave * 3.0 + noise(x, y, 16, 25) * 8.0, 2.0),
			)
		};
		let grit = if hash(x as i32, y as i32, 26) > 0.975 {
			-8.0
		} else {
			0.0
		};
		tone(base, value + band(detail * 3.0, 2.0) + grit, wave * 1.5)
	})
}

pub(super) fn snow() -> RgbImage {
	field(|x, y| {
		let drift = noise(x, y, 32, 31) * 6.0 + noise(x, y, 8, 32) * 2.0;
		let sparkle = if hash(x as i32, y as i32, 33) > 0.994 {
			5.0
		} else {
			0.0
		};
		tone([239, 244, 249], band(drift, 2.0) + sparkle, drift * 0.3)
	})
}

pub(super) fn mud() -> RgbImage {
	field(|x, y| {
		let clump = noise(x, y, 8, 41);
		let basin = noise(x, y, 16, 42);
		let sheen = if basin < -0.30 && clump.abs() < 0.09 {
			12.0
		} else {
			0.0
		};
		tone(
			[83, 64, 46],
			band(clump * 11.0 + basin * 8.0, 3.0) + sheen,
			-sheen * 0.2,
		)
	})
}

struct Cell {
	x: f32,
	y: f32,
	tone: f32,
}

fn cells(count: i32, seed: u64) -> Vec<Cell> {
	let step = SIZE as f32 / count as f32;
	(0..count * count)
		.map(|i| Cell {
			x: (i % count) as f32 * step + step * (0.15 + hash(i, 0, seed) * 0.7),
			y: (i / count) as f32 * step + step * (0.15 + hash(i, 1, seed) * 0.7),
			tone: hash(i, 2, seed) * 2.0 - 1.0,
		})
		.collect()
}

fn wrap(d: f32) -> f32 {
	(d + SIZE as f32 / 2.0).rem_euclid(SIZE as f32) - SIZE as f32 / 2.0
}

pub(super) fn aggregate(gravel: bool) -> RgbImage {
	let sites = cells(if gravel { 9 } else { 4 }, if gravel { 51 } else { 52 });
	field(|x, y| {
		let (mut first, mut second, mut which) = (f32::MAX, f32::MAX, 0);
		for (i, c) in sites.iter().enumerate() {
			let (dx, dy) = (wrap(x - c.x), wrap(y - c.y));
			let d = (dx * dx * (1.0 + c.tone * 0.2) + dy * dy).sqrt();
			if d < first {
				second = first;
				first = d;
				which = i;
			} else if d < second {
				second = d;
			}
		}
		let c = &sites[which];
		let edge = second - first;
		let chip = noise(x, y, 2, 53) * 0.4;
		let radius = if gravel { 4.0 } else { 12.0 };
		let plane = (-wrap(x - c.x) * 0.65 - wrap(y - c.y) * 0.75) / radius;
		let value = if edge + chip < if gravel { 0.6 } else { 0.65 } {
			-28.0 + noise(x, y, 4, 54) * 6.0
		} else {
			let bevel = if edge < 2.0 {
				plane * 17.0
			} else {
				band(plane * 8.0, 4.0)
			};
			c.tone * 19.0 + bevel + band(noise(x, y, 2, 55) * 5.0, 3.0)
		};
		tone(
			if gravel {
				[125, 123, 120]
			} else {
				[120, 122, 126]
			},
			value,
			c.tone * 4.0,
		)
	})
}

pub(super) fn foliage(pine: bool) -> RgbImage {
	let mut out = field(|x, y| {
		tone(
			if pine { [28, 58, 41] } else { [40, 76, 29] },
			band(noise(x, y, 8, 61) * 10.0, 4.0),
			0.0,
		)
	});
	let mut rng = Rng(if pine { 62 } else { 63 });
	for _ in 0..if pine { 110 } else { 92 } {
		let (cx, cy) = (rng.range(64) as i32, rng.range(64) as i32);
		let slope = if rng.range(2) == 0 { -1.0 } else { 1.0 };
		let size = 4.0 + rng.range(4) as f32;
		let shade = rng.signed(12) as f32;
		for dy in -10i32..=10 {
			for dx in -10i32..=10 {
				let u = (dx as f32 + slope * dy as f32) * 0.707;
				let v = (dy as f32 - slope * dx as f32) * 0.707;
				let shape = if pine {
					u.abs() / size + v.abs() / 1.7
				} else {
					(u / size).powi(2) + (v / (size * 0.5)).powi(2)
				};
				if shape > 1.0 {
					continue;
				}
				let midrib = if v.abs() < 0.6 && u.abs() < size * 0.7 {
					-5.0
				} else {
					0.0
				};
				let light = if v < -0.6 { 8.0 } else { -3.0 };
				let edge = if shape > 0.8 && dy > 0 { -9.0 } else { 0.0 };
				out.put_pixel(
					(cx + dx).rem_euclid(64) as u32,
					(cy + dy).rem_euclid(64) as u32,
					image::Rgb(tone(
						if pine { [43, 89, 62] } else { [70, 116, 41] },
						shade + light + edge + midrib,
						if pine { -2.0 } else { 2.0 },
					)),
				);
			}
		}
	}
	out
}

pub(super) fn bricks(sandy: bool) -> RgbImage {
	field(|x, y| {
		let row = y as i32 / 16;
		let xx = (x as i32 + (row % 2) * 16) % 64;
		let (bx, by) = (xx % 32, y as i32 % 16);
		let id = xx / 32;
		let variation = hash(id, row, 71) * 14.0 - 7.0;
		let edge = bx.min(31 - bx).min(by.min(15 - by));
		let chipped = edge == 1 && hash(xx / 2, y as i32 / 2, 72) > 0.79;
		let relief = if edge == 0 || chipped {
			-25.0
		} else if by == 1 || bx == 1 {
			10.0
		} else if by == 14 || bx == 30 {
			-10.0
		} else {
			0.0
		};
		let grain = band(noise(x, y, 4, 73) * 6.0 + noise(x, y, 16, 74) * 4.0, 3.0);
		tone(
			if sandy {
				[190, 166, 118]
			} else {
				[132, 134, 139]
			},
			variation + relief + grain,
			variation * 0.2,
		)
	})
}

pub(super) fn polished() -> RgbImage {
	field(|x, y| {
		let cloud = noise(x, y, 16, 81);
		let vein = (noise(x, y, 32, 82) + noise(x, y, 8, 83) * 0.15).abs();
		let mark = if vein < 0.02 {
			9.0
		} else if vein < 0.06 {
			3.0
		} else {
			0.0
		};
		tone([113, 118, 127], band(cloud * 6.0, 2.0) + mark, cloud * 2.0)
	})
}

pub(super) fn metal() -> RgbImage {
	let sites = cells(6, 91);
	field(|x, y| {
		let c = sites
			.iter()
			.min_by(|a, b| {
				let d = |c: &Cell| wrap(x - c.x).powi(2) + wrap(y - c.y).powi(2);
				d(a).total_cmp(&d(b))
			})
			.expect("hammer marks");
		let plane = (wrap(x - c.x) - wrap(y - c.y)) * c.tone * 0.5;
		let brush = noise(x, y, 2, 92) * 1.4;
		let scratch = if hash(x as i32 / 8, y as i32, 93) > 0.991 {
			7.0
		} else {
			0.0
		};
		tone(
			[139, 147, 156],
			band(c.tone * 5.0 + plane + brush, 2.0) + scratch,
			-c.tone,
		)
	})
}

#[derive(Clone, Copy)]
pub(super) enum Deposit {
	Ember,
	Dusk,
	Glow,
	Aether,
	Heart,
	Ley,
	Spring,
}

fn segment(mask: &mut [f32], a: (f32, f32), b: (f32, f32), width: f32) {
	let (vx, vy) = (b.0 - a.0, b.1 - a.1);
	for y in 0..SIZE {
		for x in 0..SIZE {
			let (dx, dy) = (wrap(x as f32 - a.0), wrap(y as f32 - a.1));
			let t = ((dx * vx + dy * vy) / (vx * vx + vy * vy).max(0.01)).clamp(0.0, 1.0);
			let d = ((dx - vx * t).powi(2) + (dy - vy * t).powi(2)).sqrt();
			let v = (width + 1.2 - d) / 1.2;
			let at = (y * SIZE + x) as usize;
			mask[at] = mask[at].max(v.clamp(0.0, 2.0));
		}
	}
}

pub(super) fn deposit(
	host: &RgbImage,
	core: [u8; 3],
	rim: [u8; 3],
	kind: Deposit,
) -> (RgbImage, RgbImage) {
	let mut mask = vec![0.0f32; (SIZE * SIZE) as usize];
	let mut rng = Rng(match kind {
		Deposit::Ember => 101,
		Deposit::Dusk => 102,
		Deposit::Glow => 103,
		Deposit::Aether => 104,
		Deposit::Heart => 105,
		Deposit::Ley => 106,
		Deposit::Spring => 107,
	});
	let count = match kind {
		Deposit::Glow | Deposit::Ley => 6,
		Deposit::Heart => 9,
		_ => 13,
	};
	for _ in 0..count {
		let (x, y) = (rng.range(64) as f32, rng.range(64) as f32);
		let slope = if rng.range(2) == 0 { -1.0 } else { 1.0 };
		match kind {
			Deposit::Ember => {
				segment(&mut mask, (x, y), (x + 3.0 * slope, y + 2.0), 1.9);
				segment(
					&mut mask,
					(x + slope, y + 1.0),
					(x - 2.0 * slope, y + 5.0),
					1.0,
				);
			}
			Deposit::Dusk => segment(&mut mask, (x, y), (x + 5.0 * slope, y - 8.0), 0.9),
			Deposit::Aether => {
				segment(&mut mask, (x, y), (x + 2.0, y - 5.0), 1.8);
				segment(&mut mask, (x + 2.0, y - 1.0), (x + 6.0, y - 2.0), 1.0);
			}
			Deposit::Heart => {
				segment(&mut mask, (x, y), (x + slope, y + 8.0), 0.7);
				segment(&mut mask, (x + slope, y + 8.0), (x, y + 13.0), 1.1);
			}
			Deposit::Spring => {
				segment(&mut mask, (x, y), (x + 2.0, y), 1.6);
				segment(&mut mask, (x, y + 1.0), (x - 5.0, y + 3.0), 0.6);
			}
			Deposit::Glow | Deposit::Ley => {
				let b = (x + 7.0 * slope, y + 6.0);
				segment(&mut mask, (x, y), b, 0.9);
				segment(&mut mask, b, (x + 5.0 * slope, y + 15.0), 1.0);
				segment(&mut mask, b, (x + 14.0 * slope, y + 3.0), 0.5);
			}
		}
	}
	let paint = |spent: bool| {
		field(|x, y| {
			let p = host.get_pixel(x as u32, y as u32).0;
			let m = mask[(y as u32 * SIZE + x as u32) as usize];
			let body = if spent {
				0.93
			} else if matches!(kind, Deposit::Heart | Deposit::Spring) {
				0.42
			} else {
				0.34
			};
			let mut rgb = p.map(|v| clamp8(f32::from(v) * body));
			if m > 0.0 {
				if m < 0.55 {
					rgb = rgb.map(|v| add8(v, -18));
				} else {
					let ink = if m > 1.35 { core } else { rim };
					let intensity = if spent {
						if m > 1.35 { 0.32 } else { 0.40 }
					} else {
						1.0
					};
					let facet = band(noise(x, y, 2, 110) * 6.0, 3.0);
					rgb = ink.map(|v| clamp8(f32::from(v) * intensity + facet));
				}
			}
			rgb
		})
	};
	(paint(false), paint(true))
}

pub(super) fn depleted(stone: &RgbImage) -> RgbImage {
	let mut out = stone.clone();
	let sites = cells(4, 115);
	for (x, y, p) in out.enumerate_pixels_mut() {
		for c in &sites {
			let (dx, dy) = (wrap(x as f32 - c.x), wrap(y as f32 - c.y));
			let d = dx.abs() + dy.abs() * 0.8;
			if d < 3.0 {
				p.0 = tone(p.0, -35.0, 0.0);
			} else if d < 4.0 {
				p.0 = tone(p.0, if dy < 0.0 { 7.0 } else { -9.0 }, 0.0);
			}
		}
	}
	out
}

pub(super) fn lamp(aether: bool) -> RgbImage {
	let (core, rim) = super::Essence::Aether.palette();
	let (pane, frame, trim) = if aether {
		(
			std::array::from_fn(|i| clamp8(f32::from(core[i]) * 0.6 + f32::from(rim[i]) * 0.4)),
			core.map(|v| clamp8(f32::from(v) * 0.12 + 35.0)),
			core.map(|v| clamp8(f32::from(v) * 0.25 + 18.0)),
		)
	} else {
		([244, 194, 115], [76, 62, 41], [84, 64, 34])
	};
	field(|x, y| {
		let edge = x.min(63.0 - x).min(y.min(63.0 - y));
		if edge < 7.0 {
			let bevel = if edge < 1.0 {
				-12.0
			} else if edge < 2.0 {
				15.0
			} else if edge > 5.0 {
				-16.0
			} else {
				0.0
			};
			let rivet = ((x - 4.0).abs().min((x - 59.0).abs()) <= 1.0)
				&& ((y - 4.0).abs().min((y - 59.0).abs()) <= 1.0);
			return tone(frame, bevel + if rivet { 22.0 } else { 0.0 }, 0.0);
		}
		let (dx, dy) = (x - 31.5, y - 31.5);
		let rib = if aether {
			(dx.abs() + dy.abs() - 22.0).abs() < 1.0
		} else {
			(x - 20.0).abs() < 1.0 || (x - 43.0).abs() < 1.0
		};
		if rib {
			return trim;
		}
		let d = (dx * dx + dy * dy).sqrt() / 35.0;
		let k = 1.0 - band(d * 0.32, 0.04);
		pane.map(|v| clamp8(f32::from(v) * k))
	})
}

pub(super) fn crystal() -> RgbaImage {
	let mut cracks = vec![0.0; (SIZE * SIZE) as usize];
	for (a, b) in [
		((9.0, 5.0), (17.0, 20.0)),
		((17.0, 20.0), (13.0, 28.0)),
		((40.0, 39.0), (48.0, 54.0)),
		((48.0, 54.0), (55.0, 58.0)),
	] {
		segment(&mut cracks, a, b, 0.2);
	}
	RgbaImage::from_fn(SIZE, SIZE, |x, y| {
		let cloud = band(noise(x as f32, y as f32, 16, 121) * 5.0, 2.0);
		let vein = cracks[(y * SIZE + x) as usize] * 22.0;
		let rgb = tone([57, 80, 118], cloud + vein, -vein * 0.3);
		image::Rgba([rgb[0], rgb[1], rgb[2], 214])
	})
}

pub(super) fn glow(core: [u8; 3], rim: [u8; 3]) -> RgbImage {
	field(|x, y| {
		let (dx, dy) = (x - 31.5, y - 31.5);
		let d = ((dx * dx + dy * dy).sqrt() / 32.0).min(1.0);
		let t = band(d, 0.08);
		std::array::from_fn(|i| clamp8(f32::from(core[i]) * (1.0 - t) + f32::from(rim[i]) * t))
	})
}

pub(super) fn torch(wood: &RgbImage) -> RgbImage {
	let mut out = wood.clone();
	let rows = SIZE * 5 / 12;
	for (x, y, p) in out.enumerate_pixels_mut() {
		if y < rows {
			let tongues = noise(x as f32, y as f32, 8, 131);
			let heat = (y as f32 / rows as f32 + tongues * 0.22).clamp(0.0, 1.0);
			p.0 = if heat < 0.25 {
				[203, 77, 19]
			} else if heat < 0.5 {
				[238, 116, 24]
			} else if heat < 0.8 {
				[255, 165, 40]
			} else {
				[255, 208, 87]
			};
		} else {
			p.0 = p.0.map(|v| add8(v, -18));
		}
	}
	out
}
