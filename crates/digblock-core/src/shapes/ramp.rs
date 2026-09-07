//! Twelfth-step ramps: a run of two or more consecutive one-twelfth steps
//! (rise one, run one) in the raster draws as one diagonal quad plus one
//! triangle per open side, instead of a tread and a riser per step. The
//! detector knows one case, treads ascending horizontally with air above;
//! soffits (a stringer's underside) and vertical wall runs (a sawtooth seen
//! from above) are the same case in a reflected or axis-swapped raster, so
//! each pass runs the detector on a transformed copy and maps the answer
//! back. A pure render-side merge over `bake_tex`'s raster: collision,
//! light and the face masks still come from the boxes, coarser steps stay
//! boxy, and a lone step never joins, so a single step down keeps its edge.
//! A ramp carries the axis dir its treads had, since the vertex format has
//! no diagonal normal: it is lit as a flat face. Triangles ride the shared
//! index pattern as quads with one corner folded onto its neighbour, which
//! leaves exactly one of the two triangles standing, wound outward.

use super::{G, ShapeQuad, boundary_rect, occ_idx, uv_pos};
use crate::dir::{DIRS, Dir};

/// Fewest risers a run needs before it joins.
const MIN_RISERS: i32 = 2;

/// What the ramp pass drew, and the face cells it drew over (per `Dir`,
/// indexed like the raster) so the greedy pass skips them.
pub(super) struct Joined {
	pub quads: Vec<ShapeQuad>,
	pub consumed: [Vec<bool>; 6],
}

/// A maximal run of twelfth steps ascending toward `d` through slices
/// [w0, w1): tread k sits at run-axis column `c0 + k * step(d)`, height
/// `h0 + k`, for k = 0..=n, so there are n risers between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Run {
	d: Dir,
	c0: i32,
	h0: i32,
	n: i32,
	w0: i32,
	w1: i32,
}

/// Run-axis step per direction: +1 toward West/North, -1 toward East/South.
fn step(d: Dir) -> i32 {
	let (ox, _, oz) = d.offset();
	ox + oz
}

/// (x, y, z) from run-axis coordinate `a`, height `y` and side-axis `w`.
fn pos(d: Dir, a: i32, y: i32, w: i32) -> [i32; 3] {
	match d {
		Dir::West | Dir::East => [a, y, w],
		_ => [w, y, a],
	}
}

/// The two side dirs of a run: along the horizontal axis it does not climb.
fn sides(d: Dir) -> [Dir; 2] {
	match d {
		Dir::West | Dir::East => [Dir::South, Dir::North],
		_ => [Dir::East, Dir::West],
	}
}

/// Lattice (u, v) of a position on the face plane toward `dir`: the inverse
/// of `uv_pos`.
pub(super) fn uv_of(dir: Dir, p: [i32; 3]) -> (i32, i32) {
	let [x, y, z] = p;
	match dir {
		Dir::West => (G - z, G - y),
		Dir::North => (x, G - y),
		Dir::South => (G - x, G - y),
		Dir::East => (z, G - y),
		Dir::Top => (G - x, G - z),
		Dir::Bottom => (G - x, z),
	}
}

/// A right triangle on the face plane toward `dir`, from three lattice
/// corners of one axis-aligned rect, as a quad with the rect's fourth
/// corner folded onto a neighbour along an edge. The fold keeps one
/// triangle of the (BR,TR,TL)+(BR,TL,BL) pattern intact, so it winds as the
/// rect would.
fn tri_quad(dir: Dir, q: i32, pts: [(i32, i32); 3], tex: Option<u16>, boundary: bool) -> ShapeQuad {
	let u0 = pts.iter().map(|p| p.0).min().unwrap();
	let u1 = pts.iter().map(|p| p.0).max().unwrap();
	let v0 = pts.iter().map(|p| p.1).min().unwrap();
	let v1 = pts.iter().map(|p| p.1).max().unwrap();
	let has = |u: i32, v: i32| pts.contains(&(u, v));
	debug_assert!(u0 < u1 && v0 < v1, "degenerate triangle {pts:?}");
	debug_assert!(
		[(u0, v0), (u1, v0), (u0, v1), (u1, v1)]
			.iter()
			.filter(|&&(u, v)| has(u, v))
			.count() == 3,
		"not a right triangle on the lattice: {pts:?}"
	);
	// [TL, TR, BL, BR]
	let mut lat = [(u0, v0), (u1, v0), (u0, v1), (u1, v1)];
	if !has(u0, v0) {
		lat[0] = lat[1];
	} else if !has(u1, v0) {
		lat[1] = lat[0];
	} else if !has(u0, v1) {
		lat[2] = lat[3];
	} else {
		lat[3] = lat[2];
	}
	let corners = lat.map(|(u, v)| uv_pos(dir, q, u, v));
	let uv = lat.map(|(u, v)| [u as u8, v as u8]);
	let rect = if boundary {
		boundary_rect(dir, &corners)
	} else {
		[0; 4]
	};
	ShapeQuad {
		dir,
		corners,
		uv,
		tex,
		boundary,
		rect,
	}
}

/// An axis permutation with per-axis reflection: `out[i]` takes input
/// axis `perm[i]`, negated (about the block) when `neg[i]`.
struct Xf {
	perm: [usize; 3],
	neg: [bool; 3],
}

impl Xf {
	const IDENTITY: Xf = Xf {
		perm: [0, 1, 2],
		neg: [false; 3],
	};
	/// Upside down: soffits become treads.
	const FLIP_Y: Xf = Xf {
		perm: [0, 1, 2],
		neg: [false, true, false],
	};
	/// y ↔ z: a sawtooth wall climbing +z becomes treads.
	const SWAP_YZ: Xf = Xf {
		perm: [0, 2, 1],
		neg: [false; 3],
	};
	/// y ↔ z and both negated: a sawtooth wall climbing -z becomes treads.
	const SWAP_YZ_NEG: Xf = Xf {
		perm: [0, 2, 1],
		neg: [false, true, true],
	};

	/// A lattice point back.
	fn lattice_back(&self, q: [i32; 3]) -> [i32; 3] {
		let mut p = [0; 3];
		for i in 0..3 {
			p[self.perm[i]] = if self.neg[i] { G - q[i] } else { q[i] };
		}
		p
	}

	/// A cell (0..G per axis) forward.
	fn cell(&self, p: [i32; 3]) -> [i32; 3] {
		std::array::from_fn(|i| {
			if self.neg[i] {
				G - 1 - p[self.perm[i]]
			} else {
				p[self.perm[i]]
			}
		})
	}

	/// A cell back.
	fn cell_back(&self, q: [i32; 3]) -> [i32; 3] {
		let mut p = [0; 3];
		for i in 0..3 {
			p[self.perm[i]] = if self.neg[i] { G - 1 - q[i] } else { q[i] };
		}
		p
	}

	/// A dir back.
	fn dir_back(&self, d: Dir) -> Dir {
		let (ox, oy, oz) = d.offset();
		let q = [ox, oy, oz];
		let mut o = [0; 3];
		for i in 0..3 {
			o[self.perm[i]] = if self.neg[i] { -q[i] } else { q[i] };
		}
		*DIRS
			.iter()
			.find(|d| d.offset() == (o[0], o[1], o[2]))
			.expect("an axis offset maps to an axis offset")
	}

	/// Determinant -1: winding reverses under this map.
	fn reflects(&self) -> bool {
		let swapped = self.perm != [0, 1, 2];
		let negs = self.neg.iter().filter(|&&n| n).count();
		swapped != (negs % 2 == 1)
	}
}

/// Find every twelfth-step run in the raster and draw it. `tex_at` answers
/// a filled cell's resolved texture for a face dir. The axis-swapped passes
/// scan x runs only: their z runs are the y-rise runs the first two passes
/// already found.
pub(super) fn join(occ: &[bool], tex_at: &dyn Fn([i32; 3], Dir) -> Option<u16>) -> Joined {
	const ALL: [Dir; 4] = [Dir::West, Dir::East, Dir::North, Dir::South];
	const X_ONLY: [Dir; 2] = [Dir::West, Dir::East];
	let passes: [(&Xf, &[Dir]); 4] = [
		(&Xf::IDENTITY, &ALL),
		(&Xf::FLIP_Y, &ALL),
		(&Xf::SWAP_YZ, &X_ONLY),
		(&Xf::SWAP_YZ_NEG, &X_ONLY),
	];
	let mut out = Joined {
		quads: Vec::new(),
		consumed: std::array::from_fn(|_| vec![false; occ.len()]),
	};
	let idx = |p: [i32; 3]| occ_idx(p[0], p[1], p[2]);
	for (xf, run_dirs) in passes {
		let mut moved = vec![false; occ.len()];
		for y in 0..G {
			for z in 0..G {
				for x in 0..G {
					moved[idx(xf.cell([x, y, z]))] = occ[idx([x, y, z])];
				}
			}
		}
		let found = join_treads(
			&moved,
			&|q, d| tex_at(xf.cell_back(q), xf.dir_back(d)),
			run_dirs,
		);
		for (d, cells) in DIRS.iter().zip(&found.consumed) {
			let dst = &mut out.consumed[xf.dir_back(*d) as usize];
			for (i, &c) in cells.iter().enumerate() {
				if c {
					let i = i as i32;
					dst[idx(xf.cell_back([i % G, i / (G * G), (i / G) % G]))] = true;
				}
			}
		}
		// A reflection reverses winding; swapping the top and bottom
		// corner slots reverses it back. The uv is re-read from the
		// corners in the final dir's frame.
		for q in found.quads {
			let m = q
				.corners
				.map(|c| xf.lattice_back(c.map(i32::from)).map(|v| v as u8));
			let corners = if xf.reflects() {
				[m[2], m[3], m[0], m[1]]
			} else {
				m
			};
			let dir = xf.dir_back(q.dir);
			let uv = corners.map(|c| {
				let (u, v) = uv_of(dir, c.map(i32::from));
				[u as u8, v as u8]
			});
			let rect = if q.boundary {
				boundary_rect(dir, &corners)
			} else {
				[0; 4]
			};
			out.quads.push(ShapeQuad {
				dir,
				corners,
				uv,
				tex: q.tex,
				boundary: q.boundary,
				rect,
			});
		}
	}
	out
}

/// The tread runs alone: air above, ascending toward `d`.
fn join_treads(
	occ: &[bool],
	tex_at: &dyn Fn([i32; 3], Dir) -> Option<u16>,
	run_dirs: &[Dir],
) -> Joined {
	let occ_at = |p: [i32; 3]| -> bool {
		p.iter().all(|c| (0..G).contains(c)) && occ[occ_idx(p[0], p[1], p[2])]
	};
	// A tread: filled with air above. A valley: a tread with a tread one
	// higher on both sides, which belongs to neither neighbour's run.
	let tread =
		|d: Dir, a: i32, h: i32, w: i32| occ_at(pos(d, a, h, w)) && !occ_at(pos(d, a, h + 1, w));
	let valley = |d: Dir, a: i32, h: i32, w: i32| {
		tread(d, a, h, w) && tread(d, a - 1, h + 1, w) && tread(d, a + 1, h + 1, w)
	};
	let mut runs: Vec<Run> = Vec::new();
	for &d in run_dirs {
		let da = step(d);
		for w in 0..G {
			for a in 0..G {
				for h in 0..G {
					if !tread(d, a, h, w) || valley(d, a, h, w) {
						continue;
					}
					// Not a continuation: the low-side column is not a
					// tread one lower, unless that tread is a valley.
					if tread(d, a - da, h - 1, w) && !valley(d, a - da, h - 1, w) {
						continue;
					}
					let mut n = 0;
					while tread(d, a + (n + 1) * da, h + n + 1, w) {
						n += 1;
					}
					if n >= MIN_RISERS {
						runs.push(Run {
							d,
							c0: a,
							h0: h,
							n,
							w0: w,
							w1: w + 1,
						});
					}
				}
			}
		}
	}
	// Merge identical runs in adjacent slices into one width.
	runs.sort_by_key(|r| (r.d as u8, r.c0, r.h0, r.n, r.w0));
	let mut merged: Vec<Run> = Vec::new();
	for r in runs {
		match merged.last_mut() {
			Some(m) if (m.d, m.c0, m.h0, m.n, m.w1) == (r.d, r.c0, r.h0, r.n, r.w0) => m.w1 = r.w1,
			_ => merged.push(r),
		}
	}
	let mut out = Joined {
		quads: Vec::new(),
		consumed: std::array::from_fn(|_| vec![false; occ.len()]),
	};
	for r in merged {
		emit(&r, &occ_at, tex_at, &mut out);
	}
	out
}

fn emit(
	r: &Run,
	occ_at: &dyn Fn([i32; 3]) -> bool,
	tex_at: &dyn Fn([i32; 3], Dir) -> Option<u16>,
	out: &mut Joined,
) {
	let Run {
		d,
		c0,
		h0,
		n,
		w0,
		w1,
	} = *r;
	let da = step(d);
	let col = |k: i32| c0 + k * da;
	// The low-side edge of tread k, where the ramp plane meets it.
	let edge = |k: i32| col(k) + i32::from(da < 0);
	let y_at = |a: i32| h0 + 1 + (a - edge(0)) * da;
	let idx = |p: [i32; 3]| occ_idx(p[0], p[1], p[2]);
	// The tread tops and risers the ramp replaces.
	for k in 0..n {
		for w in w0..w1 {
			out.consumed[Dir::Top as usize][idx(pos(d, col(k), h0 + k, w))] = true;
			out.consumed[d.opposite() as usize][idx(pos(d, col(k + 1), h0 + k + 1, w))] = true;
		}
	}
	// The ramp: the Top corner layout over the run's footprint, each corner
	// lifted to the plane. Never on a boundary: a tread with a riser above
	// it sits below y = 11, and every riser stands inside the block.
	let (lo, hi) = (edge(0).min(edge(n)), edge(0).max(edge(n)));
	let ([x0, x1], [z0, z1]) = match d {
		Dir::West | Dir::East => ([lo, hi], [w0, w1]),
		_ => ([w0, w1], [lo, hi]),
	};
	let lat = [
		(G - x1, G - z1),
		(G - x0, G - z1),
		(G - x1, G - z0),
		(G - x0, G - z0),
	];
	let corners = lat.map(|(u, v)| {
		let p = uv_pos(Dir::Top, 0, u, v);
		let a = match d {
			Dir::West | Dir::East => i32::from(p[0]),
			_ => i32::from(p[2]),
		};
		[p[0], y_at(a) as u8, p[2]]
	});
	out.quads.push(ShapeQuad {
		dir: Dir::Top,
		corners,
		uv: lat.map(|(u, v)| [u as u8, v as u8]),
		tex: tex_at(pos(d, c0, h0, w0), Dir::Top),
		boundary: false,
		rect: [0; 4],
	});
	// Each side: the wedge under the plane needs a triangle wherever it
	// meets air. The support under the first `m` steps that is solid and
	// one texture draws as a single triangle; every open step past it
	// gets a small one of its own.
	for s in sides(d) {
		let (sx, _, sz) = s.offset();
		let (q, w_own, w_nb) = if sx + sz > 0 {
			(w1, w1 - 1, w1)
		} else {
			(w0, w0, w0 - 1)
		};
		let boundary = q == 0 || q == G;
		let open = |a: i32, y: i32| !occ_at(pos(d, a, y, w_nb));
		let tex = tex_at(pos(d, c0, h0, w_own), s);
		// Column k's support cells in the band: (c_k, h0 + j), j = 1..=k.
		let column_ok = |k: i32| {
			(1..=k).all(|j| {
				let p = pos(d, col(k), h0 + j, w_own);
				occ_at(p) && tex_at(p, s) == tex
			})
		};
		let m = (1..n).take_while(|&k| column_ok(k)).count() as i32 + 1;
		let tri = |pts: [(i32, i32); 3], tex: Option<u16>| {
			tri_quad(
				s,
				q,
				pts.map(|(a, y)| uv_of(s, pos(d, a, y, q))),
				tex,
				boundary,
			)
		};
		let mut from = 0;
		if m >= 2 {
			let any_open = (0..m).any(|k| open(col(k), h0 + k + 1))
				|| (1..m).any(|k| (1..=k).any(|j| open(col(k), h0 + j)));
			if any_open {
				for k in 1..m {
					for j in 1..=k {
						out.consumed[s as usize][idx(pos(d, col(k), h0 + j, w_own))] = true;
					}
				}
				out.quads.push(tri(
					[(edge(0), h0 + 1), (edge(m), h0 + m + 1), (edge(m), h0 + 1)],
					tex,
				));
			}
			from = m;
		}
		for k in from..n {
			if !open(col(k), h0 + k + 1) {
				continue;
			}
			let y = h0 + k + 1;
			let tex = tex_at(pos(d, col(k), h0 + k, w_own), s);
			out.quads.push(tri(
				[(edge(k), y), (edge(k + 1), y + 1), (edge(k + 1), y)],
				tex,
			));
		}
	}
}
