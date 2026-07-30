//! Verbatim port of the 2D path of Stefan Gustavson's public-domain
//! simplex noise (2012-03-09 version, Peter Eastman optimizations) — the
//! exact implementation the Java DigBlock shipped. Deliberately NOT a
//! `noise`-crate dependency: gradient tables and constants differ between
//! implementations, and terrain must be bit-identical to the original.
//! Verified bit-exact against the Java by `tests/golden.rs`.

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

#[inline]
fn fastfloor(x: f64) -> i32 {
	let xi = x as i32;
	if x < xi as f64 {
		xi - 1
	} else {
		xi
	}
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

	let x1 = x0 - f64::from(i1) + G2;
	let y1 = y0 - f64::from(j1) + G2;
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

#[cfg(test)]
mod tests {
	// The Java computes F2/G2 at class-init from sqrt; make sure our
	// literals are the same bits.
	#[test]
	fn skew_constants_match() {
		assert_eq!(super::F2.to_bits(), (0.5 * (3.0f64.sqrt() - 1.0)).to_bits());
		assert_eq!(super::G2.to_bits(), ((3.0 - 3.0f64.sqrt()) / 6.0).to_bits());
	}
}
