//! Port of the 2D path of Stefan Gustavson's public-domain simplex noise
//! (2012-03-09 version, Peter Eastman optimizations) — the implementation
//! the Java DigBlock shipped.

const GRAD3: [(f64, f64); 12] = [
	(1.0, 1.0),
	(-1.0, 1.0),
	(1.0, -1.0),
	(-1.0, -1.0),
	(1.0, 0.0),
	(-1.0, 0.0),
	(1.0, 0.0),
	(-1.0, 0.0),
	(0.0, 1.0),
	(0.0, -1.0),
	(0.0, 1.0),
	(0.0, -1.0),
];

#[rustfmt::skip]
const P: [u8; 256] = [
	151,160,137,91,90,15,
	131,13,201,95,96,53,194,233,7,225,140,36,103,30,69,142,8,99,37,240,21,10,23,
	190,6,148,247,120,234,75,0,26,197,62,94,252,219,203,117,35,11,32,57,177,33,
	88,237,149,56,87,174,20,125,136,171,168,68,175,74,165,71,134,139,48,27,166,
	77,146,158,231,83,111,229,122,60,211,133,230,220,105,92,41,55,46,245,40,244,
	102,143,54,65,25,63,161,1,216,80,73,209,76,132,187,208,89,18,169,200,196,
	135,130,116,188,159,86,164,100,109,198,173,186,3,64,52,217,226,250,124,123,
	5,202,38,147,118,126,255,82,85,212,207,206,59,227,47,16,58,17,182,189,28,42,
	223,183,170,213,119,248,152,2,44,154,163,70,221,153,101,155,167,43,172,9,
	129,22,39,253,19,98,108,110,79,113,224,232,178,185,112,104,218,246,97,228,
	251,34,242,193,238,210,144,12,191,179,162,241,81,51,145,235,249,14,239,107,
	49,192,214,31,181,199,106,157,184,84,204,176,115,121,50,45,127,4,150,254,
	138,236,205,93,222,114,67,29,24,72,243,141,128,195,78,66,215,61,156,180,
];

// Doubled tables, as in the Java static initializer.
const fn build_perm() -> [u8; 512] {
	let mut t = [0u8; 512];
	let mut i = 0;
	while i < 512 {
		t[i] = P[i & 255];
		i += 1;
	}
	t
}

const fn build_perm_mod12() -> [u8; 512] {
	let mut t = [0u8; 512];
	let mut i = 0;
	while i < 512 {
		t[i] = P[i & 255] % 12;
		i += 1;
	}
	t
}

const PERM: [u8; 512] = build_perm();
const PERM_MOD12: [u8; 512] = build_perm_mod12();

// Java: F2 = 0.5*(sqrt(3)-1), G2 = (3-sqrt(3))/6 — computed at class-init
// with doubles; these literals are the exact f64 values.
const F2: f64 = 0.366_025_403_784_438_6;
const G2: f64 = 0.211_324_865_405_187_13;
// 3D skew factors: F3 = 1/3, G3 = 1/6 (Gustavson 2012-03-09).
const F3: f64 = 1.0 / 3.0;
const G3: f64 = 1.0 / 6.0;

/// Full 3-component gradient set for the 3D path (GRAD3 above is its 2D
/// projection, kept separate to preserve the Java-bit-exact 2D noise).
const GRAD3D: [(f64, f64, f64); 12] = [
	(1.0, 1.0, 0.0),
	(-1.0, 1.0, 0.0),
	(1.0, -1.0, 0.0),
	(-1.0, -1.0, 0.0),
	(1.0, 0.0, 1.0),
	(-1.0, 0.0, 1.0),
	(1.0, 0.0, -1.0),
	(-1.0, 0.0, -1.0),
	(0.0, 1.0, 1.0),
	(0.0, -1.0, 1.0),
	(0.0, 1.0, -1.0),
	(0.0, -1.0, -1.0),
];

#[inline]
fn fastfloor(x: f64) -> i32 {
	let xi = x as i32;
	if x < xi as f64 { xi - 1 } else { xi }
}

#[inline]
fn dot(g: (f64, f64), x: f64, y: f64) -> f64 {
	g.0 * x + g.1 * y
}

/// 2D simplex noise in [-1, 1]. Bit-identical to the Java original.
pub fn noise2(xin: f64, yin: f64) -> f64 {
	let s = (xin + yin) * F2;
	let i = fastfloor(xin + s);
	let j = fastfloor(yin + s);
	let t = f64::from(i + j) * G2;
	let x0 = xin - (f64::from(i) - t);
	let y0 = yin - (f64::from(j) - t);

	let (i1, j1) = if x0 > y0 { (1, 0) } else { (0, 1) };

	let x1 = x0 - i1 as f64 + G2;
	let y1 = y0 - j1 as f64 + G2;
	let x2 = x0 - 1.0 + 2.0 * G2;
	let y2 = y0 - 1.0 + 2.0 * G2;

	let ii = (i & 255) as usize;
	let jj = (j & 255) as usize;
	let gi0 = PERM_MOD12[ii + PERM[jj] as usize] as usize;
	let gi1 = PERM_MOD12[ii + i1 as usize + PERM[jj + j1 as usize] as usize] as usize;
	let gi2 = PERM_MOD12[ii + 1 + PERM[jj + 1] as usize] as usize;

	let mut n = 0.0;
	let t0 = 0.5 - x0 * x0 - y0 * y0;
	if t0 >= 0.0 {
		let t0 = t0 * t0;
		n += t0 * t0 * dot(GRAD3[gi0], x0, y0);
	}
	let t1 = 0.5 - x1 * x1 - y1 * y1;
	if t1 >= 0.0 {
		let t1 = t1 * t1;
		n += t1 * t1 * dot(GRAD3[gi1], x1, y1);
	}
	let t2 = 0.5 - x2 * x2 - y2 * y2;
	if t2 >= 0.0 {
		let t2 = t2 * t2;
		n += t2 * t2 * dot(GRAD3[gi2], x2, y2);
	}
	70.0 * n
}

/// 3D simplex noise in [-1, 1] — the 3D path of the same Gustavson
/// implementation the 2D port came from. Used for volumetric worldgen
/// (cave carving; ores later).
pub fn noise3(xin: f64, yin: f64, zin: f64) -> f64 {
	let s = (xin + yin + zin) * F3;
	let i = fastfloor(xin + s);
	let j = fastfloor(yin + s);
	let k = fastfloor(zin + s);
	let t = f64::from(i + j + k) * G3;
	let x0 = xin - (f64::from(i) - t);
	let y0 = yin - (f64::from(j) - t);
	let z0 = zin - (f64::from(k) - t);
	let (ii, jj, kk) = ((i & 255) as usize, (j & 255) as usize, (k & 255) as usize);
	corner_sum(x0, y0, z0, |c| grad_at(ii, jj, kk, c))
}

/// Gradient of lattice corner `c` (`di<<2 | dj<<1 | dk`) of the cell whose
/// wrapped lattice coordinates are `ii/jj/kk`.
#[inline]
fn grad_at(ii: usize, jj: usize, kk: usize, c: usize) -> (f64, f64, f64) {
	let (di, dj, dk) = (c >> 2, (c >> 1) & 1, c & 1);
	let gi = PERM_MOD12[ii + di + PERM[jj + dj + PERM[kk + dk] as usize] as usize];
	GRAD3D[gi as usize]
}

/// The four-corner simplex sum for a displacement inside one lattice
/// cell. `grad` resolves a corner index — a permutation walk for the
/// one-shot path, a cached array read for [`Noise3Column`] — so both
/// paths run identical float arithmetic in identical order.
#[inline]
fn corner_sum(x0: f64, y0: f64, z0: f64, grad: impl Fn(usize) -> (f64, f64, f64)) -> f64 {
	// Rank the coordinates to pick the simplex corner traversal order.
	// Each axis scores how many of the other two it dominates (ties broken
	// exactly as Gustavson's if/else ladder does): rank 2 steps at the
	// first corner, rank ≥ 1 at the second. Branchless because the six
	// orderings are equiprobable — the ladder mispredicts on most cells.
	let rx = usize::from(x0 >= y0) + usize::from(x0 >= z0);
	let ry = usize::from(y0 > x0) + usize::from(y0 >= z0);
	let rz = usize::from(z0 > x0) + usize::from(z0 > y0);
	let (i1, j1, k1) = (
		usize::from(rx == 2),
		usize::from(ry == 2),
		usize::from(rz == 2),
	);
	let (i2, j2, k2) = (
		usize::from(rx >= 1),
		usize::from(ry >= 1),
		usize::from(rz >= 1),
	);

	let x1 = x0 - i1 as f64 + G3;
	let y1 = y0 - j1 as f64 + G3;
	let z1 = z0 - k1 as f64 + G3;
	let x2 = x0 - i2 as f64 + 2.0 * G3;
	let y2 = y0 - j2 as f64 + 2.0 * G3;
	let z2 = z0 - k2 as f64 + 2.0 * G3;
	let x3 = x0 - 1.0 + 3.0 * G3;
	let y3 = y0 - 1.0 + 3.0 * G3;
	let z3 = z0 - 1.0 + 3.0 * G3;

	let corners = [
		(x0, y0, z0, 0),
		(x1, y1, z1, i1 << 2 | j1 << 1 | k1),
		(x2, y2, z2, i2 << 2 | j2 << 1 | k2),
		(x3, y3, z3, 7),
	];

	let mut n = 0.0;
	for (x, y, z, c) in corners {
		// Clamping the falloff instead of branching on it: a clamped corner
		// contributes ±0.0, and `n` can never be -0.0 (it starts at +0.0),
		// so the sum is bit-identical to skipping the corner.
		let t = 0.6 - x * x - y * y - z * z;
		let t = if t > 0.0 { t * t } else { 0.0 };
		let (gx, gy, gz) = grad(c);
		n += t * t * (gx * x + gy * y + gz * z);
	}
	32.0 * n
}

/// Everything in one lattice corner's contribution that doesn't depend on
/// the y input. Each field is a single operation of the reference
/// expression, so folding them in keeps the arithmetic bit-identical.
#[derive(Clone, Copy)]
struct Corner {
	/// `0.6 - x²`, the falloff with its x term already subtracted.
	a: f64,
	/// `z²`, the falloff's last term.
	zq: f64,
	/// The gradient dotted with the fixed displacements, and its y
	/// component (the only one the y input still needs).
	gxx: f64,
	gy: f64,
	gzz: f64,
}

/// A vertical walk of [`noise3`] at fixed x/z. The simplex skew is `(x+y+z)/3`, so stepping y by
/// a small increment leaves the lattice cell unchanged for a long run, and inside one cell the
/// corner gradients and every x/z term of the corner sum are constant. `at(y)` hoists exactly
/// those, one operation at a time, so it stays bit-identical to `noise3(x, y, z)`
/// (`column_walk_matches_noise3` pins that).
pub struct Noise3Column {
	x: f64,
	z: f64,
	/// Lattice cell the cached state below belongs to. No finite noise
	/// input floors to `i32::MIN`, so the initial value can never hit.
	cell: [i32; 3],
	x0: f64,
	z0: f64,
	/// `j - t`: the y input minus this is the corner displacement.
	j_off: f64,
	/// The cell's 8 corners, indexed `di<<2 | dj<<1 | dk`.
	corners: [Corner; 8],
}

impl Noise3Column {
	pub fn new(x: f64, z: f64) -> Self {
		Self {
			x,
			z,
			cell: [i32::MIN; 3],
			x0: 0.0,
			z0: 0.0,
			j_off: 0.0,
			corners: [Corner {
				a: 0.0,
				zq: 0.0,
				gxx: 0.0,
				gy: 0.0,
				gzz: 0.0,
			}; 8],
		}
	}

	fn enter(&mut self, i: i32, j: i32, k: i32) {
		let t = f64::from(i + j + k) * G3;
		let x0 = self.x - (f64::from(i) - t);
		let z0 = self.z - (f64::from(k) - t);
		self.x0 = x0;
		self.z0 = z0;
		self.j_off = f64::from(j) - t;
		let (ii, jj, kk) = ((i & 255) as usize, (j & 255) as usize, (k & 255) as usize);
		for (c, e) in self.corners.iter_mut().enumerate() {
			let (di, dj, dk) = (c >> 2, (c >> 1) & 1, c & 1);
			// Corner 0 is the displacement itself; every other corner steps
			// by its offsets and unskews by one G3 per step.
			let g3 = (di + dj + dk) as f64 * G3;
			let (cx, cz) = if c == 0 {
				(x0, z0)
			} else {
				(x0 - di as f64 + g3, z0 - dk as f64 + g3)
			};
			let (gx, gy, gz) = grad_at(ii, jj, kk, c);
			*e = Corner {
				a: 0.6 - cx * cx,
				zq: cz * cz,
				gxx: gx * cx,
				gy,
				gzz: gz * cz,
			};
		}
		self.cell = [i, j, k];
	}

	pub fn at(&mut self, y: f64) -> f64 {
		let s = (self.x + y + self.z) * F3;
		let i = fastfloor(self.x + s);
		let j = fastfloor(y + s);
		let k = fastfloor(self.z + s);
		if [i, j, k] != self.cell {
			self.enter(i, j, k);
		}
		let (x0, z0) = (self.x0, self.z0);
		let y0 = y - self.j_off;

		let rx = usize::from(x0 >= y0) + usize::from(x0 >= z0);
		let ry = usize::from(y0 > x0) + usize::from(y0 >= z0);
		let rz = usize::from(z0 > x0) + usize::from(z0 > y0);
		let c1 = usize::from(rx == 2) << 2 | usize::from(ry == 2) << 1 | usize::from(rz == 2);
		let c2 = usize::from(rx >= 1) << 2 | usize::from(ry >= 1) << 1 | usize::from(rz >= 1);

		// Subtracting the j offset is either `y0 - 1.0` or a no-op —
		// `y0 - 0.0` is y0 down to the bit, so the second case needs no
		// subtraction and neither needs an int-to-float conversion.
		let ym = y0 - 1.0;
		let y1 = (if c1 & 2 != 0 { ym } else { y0 }) + G3;
		let y2 = (if c2 & 2 != 0 { ym } else { y0 }) + 2.0 * G3;
		let y3 = ym + 3.0 * G3;

		let mut n = 0.0;
		for (cy, c) in [(y0, 0), (y1, c1), (y2, c2), (y3, 7)] {
			let e = &self.corners[c];
			let t = (e.a - cy * cy) - e.zq;
			let t = if t > 0.0 { t * t } else { 0.0 };
			n += t * t * ((e.gxx + e.gy * cy) + e.gzz);
		}
		32.0 * n
	}
}

/// Gustavson's 3D path transcribed verbatim: permutation walk per call,
/// if/else corner ladder, branch on the falloff. Every shipped path here
/// only hoists and reassociates nothing, so all of them must agree with
/// this bit for bit — it is the independent witness the cave tests
/// compare against, and a mismatch is a worldgen output change.
#[cfg(test)]
pub(crate) mod reference {
	use super::{F3, G3, GRAD3D, PERM, PERM_MOD12, fastfloor};

	#[allow(clippy::collapsible_else_if)]
	pub(crate) fn noise3(xin: f64, yin: f64, zin: f64) -> f64 {
		let s = (xin + yin + zin) * F3;
		let i = fastfloor(xin + s);
		let j = fastfloor(yin + s);
		let k = fastfloor(zin + s);
		let t = f64::from(i + j + k) * G3;
		let x0 = xin - (f64::from(i) - t);
		let y0 = yin - (f64::from(j) - t);
		let z0 = zin - (f64::from(k) - t);
		let ((i1, j1, k1), (i2, j2, k2)) = if x0 >= y0 {
			if y0 >= z0 {
				((1, 0, 0), (1, 1, 0))
			} else if x0 >= z0 {
				((1, 0, 0), (1, 0, 1))
			} else {
				((0, 0, 1), (1, 0, 1))
			}
		} else {
			if y0 < z0 {
				((0, 0, 1), (0, 1, 1))
			} else if x0 < z0 {
				((0, 1, 0), (0, 1, 1))
			} else {
				((0, 1, 0), (1, 1, 0))
			}
		};
		let x1 = x0 - i1 as f64 + G3;
		let y1 = y0 - j1 as f64 + G3;
		let z1 = z0 - k1 as f64 + G3;
		let x2 = x0 - i2 as f64 + 2.0 * G3;
		let y2 = y0 - j2 as f64 + 2.0 * G3;
		let z2 = z0 - k2 as f64 + 2.0 * G3;
		let x3 = x0 - 1.0 + 3.0 * G3;
		let y3 = y0 - 1.0 + 3.0 * G3;
		let z3 = z0 - 1.0 + 3.0 * G3;
		let ii = (i & 255) as usize;
		let jj = (j & 255) as usize;
		let kk = (k & 255) as usize;
		let gi = |di: usize, dj: usize, dk: usize| -> usize {
			PERM_MOD12[ii + di + PERM[jj + dj + PERM[kk + dk] as usize] as usize] as usize
		};
		let corners = [
			(x0, y0, z0, gi(0, 0, 0)),
			(x1, y1, z1, gi(i1, j1, k1)),
			(x2, y2, z2, gi(i2, j2, k2)),
			(x3, y3, z3, gi(1, 1, 1)),
		];
		let mut n = 0.0;
		for (x, y, z, g) in corners {
			let t = 0.6 - x * x - y * y - z * z;
			if t >= 0.0 {
				let t = t * t;
				let (gx, gy, gz) = GRAD3D[g];
				n += t * t * (gx * x + gy * y + gz * z);
			}
		}
		32.0 * n
	}
}

/// Fractal (fBm) sum of `octaves` of `noise2`: frequency doubles and
/// amplitude halves per octave, normalized back to [-1, 1]. `seed` is a
/// domain offset (the same convention the terrain heightmap uses); each
/// octave is offset further so octaves don't share zero crossings.
pub fn fbm2(x: f64, z: f64, freq: f64, octaves: u32, seed: f64) -> f64 {
	let mut sum = 0.0;
	let mut norm = 0.0;
	let mut amp = 1.0;
	let mut f = freq;
	for i in 0..octaves {
		let o = seed + f64::from(i) * 1013.0;
		sum += amp * noise2(o + x * f, o + z * f);
		norm += amp;
		amp *= 0.5;
		f *= 2.0;
	}
	sum / norm
}

#[cfg(test)]
mod tests {
	// The Java computes F2/G2 at class-init from sqrt; make sure our
	// literals are the same bits.
	#[test]
	fn skew_constants_match() {
		assert_eq!(super::F2.to_bits(), (0.5 * (3.0f64.sqrt() - 1.0)).to_bits());
		assert_eq!(super::G2.to_bits(), ((3.0 - 3.0f64.sqrt()) / 6.0).to_bits());
	}

	/// `Noise3Column` hoists per-lattice-cell work out of a vertical walk;
	/// it must be bit-identical to the one-shot path at every step, in any
	/// order, including the steps that cross a cell boundary. Sampled at
	/// the two cave frequencies (the walk's real callers) and at a coarse
	/// step that crosses a cell on every call.
	#[test]
	fn column_walk_matches_noise3() {
		for &step in &[0.045, 0.02, 0.37] {
			for xi in -20..20 {
				for zi in -13..13 {
					let (x, z) = (f64::from(xi) * 0.02 + 1619.3, f64::from(zi) * 0.02 - 274.9);
					let mut col = super::Noise3Column::new(x, z);
					for yi in 0..240 {
						let y = f64::from(yi) * step - 3.0;
						assert_eq!(
							col.at(y).to_bits(),
							super::noise3(x, y, z).to_bits(),
							"walk at ({x}, {y}, {z})"
						);
					}
					// Out-of-order probes still hit the same values.
					for yi in [200, 3, 199, 4, 3] {
						let y = f64::from(yi) * step - 3.0;
						assert_eq!(col.at(y).to_bits(), super::noise3(x, y, z).to_bits());
					}
				}
			}
		}
	}

	#[test]
	fn noise3_in_range_and_deterministic() {
		let mut min = f64::MAX;
		let mut max = f64::MIN;
		for i in 0..4000 {
			// Deterministic scattered sample points.
			let x = f64::from(i) * 0.731 - 1500.0;
			let y = f64::from(i % 61) * 1.37;
			let z = f64::from(i % 253) * 2.11 - 250.0;
			let n = super::noise3(x, y, z);
			assert_eq!(n, super::noise3(x, y, z), "deterministic");
			assert!((-1.0..=1.0).contains(&n), "in range: {n}");
			min = min.min(n);
			max = max.max(n);
		}
		assert!(min < -0.3 && max > 0.3, "spans a real range: {min}..{max}");
	}

	#[test]
	fn fbm_in_range_and_deterministic() {
		for &(x, z) in &[(0.0, 0.0), (13.7, -42.1), (-999.0, 5.5), (1e4, 1e4)] {
			let a = super::fbm2(x, z, 0.01, 4, 12.34);
			let b = super::fbm2(x, z, 0.01, 4, 12.34);
			assert_eq!(a, b, "deterministic");
			assert!((-1.0..=1.0).contains(&a), "in range: {a}");
		}
		// Different seeds decorrelate.
		let a = super::fbm2(7.0, 7.0, 0.01, 2, 1.0);
		let b = super::fbm2(7.0, 7.0, 0.01, 2, 2000.0);
		assert_ne!(a, b);
	}
}
