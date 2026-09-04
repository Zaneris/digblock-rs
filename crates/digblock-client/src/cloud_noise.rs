//! The cloud shape volume: a small tiling Perlin-Worley density the sky pass samples along its
//! march (sky.wgsl). Baked on the CPU when the renderer builds, deterministic, so nothing is
//! committed and no launcher needs a file. Perlin gives the billowing body, inverted Worley the
//! cauliflower lumps; remapping one into the other is the standard Schneider recipe.

/// Texels per axis; the volume tiles, so the sky scales it to whatever repeat it likes.
pub(crate) const SIZE: usize = 64;

fn hash(x: i32, y: i32, z: i32, seed: u32) -> u32 {
	let mut h = (x as u32).wrapping_mul(0x8da6_b343)
		^ (y as u32).wrapping_mul(0xd816_3841)
		^ (z as u32).wrapping_mul(0xcb1a_b31f)
		^ seed.wrapping_mul(0x9e37_79b9);
	h ^= h >> 15;
	h = h.wrapping_mul(0x2c1b_3c6d);
	h ^= h >> 12;
	h = h.wrapping_mul(0x297a_2d39);
	h ^ (h >> 15)
}

fn unit(h: u32) -> f32 {
	(h >> 8) as f32 / 16_777_216.0
}

const GRADS: [[f32; 3]; 12] = [
	[1.0, 1.0, 0.0],
	[-1.0, 1.0, 0.0],
	[1.0, -1.0, 0.0],
	[-1.0, -1.0, 0.0],
	[1.0, 0.0, 1.0],
	[-1.0, 0.0, 1.0],
	[1.0, 0.0, -1.0],
	[-1.0, 0.0, -1.0],
	[0.0, 1.0, 1.0],
	[0.0, -1.0, 1.0],
	[0.0, 1.0, -1.0],
	[0.0, -1.0, -1.0],
];

fn fade(t: f32) -> f32 {
	t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
	a + (b - a) * t
}

/// Tiling gradient noise at `p` (in cells) with a period of `n` cells per axis; about [-1, 1].
fn perlin(p: [f32; 3], n: i32, seed: u32) -> f32 {
	let c = p.map(f32::floor);
	let f = [p[0] - c[0], p[1] - c[1], p[2] - c[2]];
	let u = f.map(fade);
	let corner = |dx: i32, dy: i32, dz: i32| {
		let h = hash(
			(c[0] as i32 + dx).rem_euclid(n),
			(c[1] as i32 + dy).rem_euclid(n),
			(c[2] as i32 + dz).rem_euclid(n),
			seed,
		);
		let g = GRADS[(h % 12) as usize];
		g[0] * (f[0] - dx as f32) + g[1] * (f[1] - dy as f32) + g[2] * (f[2] - dz as f32)
	};
	let x00 = lerp(corner(0, 0, 0), corner(1, 0, 0), u[0]);
	let x10 = lerp(corner(0, 1, 0), corner(1, 1, 0), u[0]);
	let x01 = lerp(corner(0, 0, 1), corner(1, 0, 1), u[0]);
	let x11 = lerp(corner(0, 1, 1), corner(1, 1, 1), u[0]);
	lerp(lerp(x00, x10, u[1]), lerp(x01, x11, u[1]), u[2])
}

/// Tiling inverted Worley at `p` (in cells): 1 on a feature point, 0 a cell away.
fn worley(p: [f32; 3], n: i32, seed: u32) -> f32 {
	let c = p.map(|v| v.floor() as i32);
	let mut best = f32::MAX;
	for dz in -1..=1 {
		for dy in -1..=1 {
			for dx in -1..=1 {
				let cell = [c[0] + dx, c[1] + dy, c[2] + dz];
				let w = [
					cell[0].rem_euclid(n),
					cell[1].rem_euclid(n),
					cell[2].rem_euclid(n),
				];
				let feat = [
					cell[0] as f32 + unit(hash(w[0], w[1], w[2], seed)),
					cell[1] as f32 + unit(hash(w[0], w[1], w[2], seed + 1)),
					cell[2] as f32 + unit(hash(w[0], w[1], w[2], seed + 2)),
				];
				let d = [p[0] - feat[0], p[1] - feat[1], p[2] - feat[2]];
				best = best.min(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
			}
		}
	}
	(1.0 - best.sqrt()).clamp(0.0, 1.0)
}

/// The baked volume, `SIZE` cubed R8 texels, x fastest then y then z (the texture upload order).
pub(crate) fn volume() -> Vec<u8> {
	let n = SIZE;
	let mut out = Vec::with_capacity(n * n * n);
	for z in 0..n {
		for y in 0..n {
			for x in 0..n {
				let q = [
					x as f32 / n as f32,
					y as f32 / n as f32,
					z as f32 / n as f32,
				];
				let cells = |k: i32| q.map(|v| v * k as f32);
				let p = perlin(cells(4), 4, 1) * 0.5
					+ perlin(cells(8), 8, 2) * 0.25
					+ perlin(cells(16), 16, 3) * 0.125;
				let p01 = (0.5 + 0.5 * p / 0.875).clamp(0.0, 1.0);
				let w = worley(cells(4), 4, 11) * 0.625 + worley(cells(8), 8, 12) * 0.375;
				let pw = w + p01 * (1.0 - w);
				out.push((pw.clamp(0.0, 1.0) * 255.0).round() as u8);
			}
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The sky tiles this volume across the deck, so a step across the wrap seam no larger than
	/// any interior step is what keeps a grid of straight lines out of the clouds.
	#[test]
	fn the_volume_tiles_without_a_seam() {
		let n = SIZE;
		let v = volume();
		let at = |x: usize, y: usize, z: usize| v[(z * n + y) * n + x] as i32;
		let mut interior = 0;
		let mut seam = 0;
		for z in 0..n {
			for y in 0..n {
				for x in 0..n {
					let here = at(x, y, z);
					let step = |nx: usize, ny: usize, nz: usize| (at(nx, ny, nz) - here).abs();
					let (sx, sy, sz) = (
						step((x + 1) % n, y, z),
						step(x, (y + 1) % n, z),
						step(x, y, (z + 1) % n),
					);
					if x + 1 == n {
						seam = seam.max(sx)
					} else {
						interior = interior.max(sx)
					}
					if y + 1 == n {
						seam = seam.max(sy)
					} else {
						interior = interior.max(sy)
					}
					if z + 1 == n {
						seam = seam.max(sz)
					} else {
						interior = interior.max(sz)
					}
				}
			}
		}
		assert!(
			seam <= interior,
			"wrap step {seam} exceeds the interior's {interior}: a seam"
		);
	}
}
