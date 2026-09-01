//! The shape editor's document: one 12³ attribute grid per footprint cell,
//! sculpted with a snapped brush at a chosen resolution (halves, thirds,
//! quarters, sixths, twelfths — all exact on the twelve-grid) and compiled
//! back to a minimal `.shape` box list by a deterministic greedy merge.
//! Pure data, no engine deps — unit-tested headless.

use digblock_core::shapes::def::{DefBox, Roles, ShapeDef, StateModel};
use digblock_core::shapes::{GRID, SubBox};

const G: i32 = GRID as i32;
const N: usize = (GRID as usize).pow(3);

/// One sub-voxel: role bits (0 = empty) and the box's texture triple
/// (resolved top/side/bottom layers, `None` = the block row's default).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Voxel {
	pub roles: Roles,
	pub tex: [Option<u16>; 3],
}

impl Voxel {
	pub(crate) const EMPTY: Voxel = Voxel {
		roles: Roles {
			visual: false,
			collision: false,
			light: false,
			interaction: false,
		},
		tex: [None; 3],
	};

	#[inline]
	pub(crate) fn filled(&self) -> bool {
		let r = self.roles;
		r.visual || r.collision || r.light || r.interaction
	}
}

/// One footprint cell's 12³ grid, `x + 12*(z + 12*y)` order.
#[derive(Clone, PartialEq)]
pub(crate) struct VoxelGrid(pub Box<[Voxel; N]>);

impl VoxelGrid {
	fn empty() -> Self {
		VoxelGrid(Box::new([Voxel::EMPTY; N]))
	}
}

#[inline]
fn gidx(x: i32, y: i32, z: i32) -> usize {
	(x + G * (z + G * y)) as usize
}

/// The working document. `cells` is indexed by the canonical part order
/// (`part = cx + W*(cz + D*cy)`, the same one the game derives cells with);
/// non-`Explicit` models hold one volume, `Explicit(n)` holds n.
pub(crate) struct ShapeDoc {
	pub name: String,
	pub footprint: [u8; 3],
	pub states: StateModel,
	pub ramp: bool,
	/// One volume per authored state; each volume one grid per cell.
	pub volumes: Vec<Vec<VoxelGrid>>,
	/// Which volume edits land in (always 0 outside `Explicit`).
	pub active: usize,
	pub dirty: bool,
}

impl ShapeDoc {
	/// A fresh single-cell facing-model document with nothing in it.
	pub(crate) fn new(name: &str) -> Self {
		ShapeDoc {
			name: name.into(),
			footprint: [1, 1, 1],
			states: StateModel::Facing,
			ramp: false,
			volumes: vec![vec![VoxelGrid::empty()]],
			active: 0,
			dirty: false,
		}
	}

	pub(crate) fn parts(&self) -> u8 {
		self.footprint[0] * self.footprint[1] * self.footprint[2]
	}

	/// Canonical cell offset of a part (the game's part order).
	pub(crate) fn part_cell(&self, part: u8) -> (i32, i32, i32) {
		let [w, _, d] = self.footprint;
		(
			i32::from(part % w),
			i32::from(part / (w * d)),
			i32::from((part / w) % d),
		)
	}

	/// Whole-volume sub-voxel extents (W*12, H*12, D*12).
	pub(crate) fn extent(&self) -> [i32; 3] {
		[
			i32::from(self.footprint[0]) * G,
			i32::from(self.footprint[1]) * G,
			i32::from(self.footprint[2]) * G,
		]
	}

	/// The part a whole-volume sub-voxel falls in, plus its cell-local
	/// coordinates. `None` outside the volume.
	fn locate(&self, sv: [i32; 3]) -> Option<(usize, i32, i32, i32)> {
		let e = self.extent();
		if sv.iter().zip(e).any(|(&c, m)| c < 0 || c >= m) {
			return None;
		}
		let (cx, cy, cz) = (sv[0] / G, sv[1] / G, sv[2] / G);
		let [w, _, d] = self.footprint;
		let part = cx + i32::from(w) * (cz + i32::from(d) * cy);
		Some((part as usize, sv[0] % G, sv[1] % G, sv[2] % G))
	}

	/// Whether a whole-volume sub-voxel holds material (the active volume).
	pub(crate) fn occupied(&self, sv: [i32; 3]) -> bool {
		self.locate(sv)
			.is_some_and(|(p, x, y, z)| self.volumes[self.active][p].0[gidx(x, y, z)].filled())
	}

	/// Snap a coordinate down to the brush lattice.
	#[inline]
	pub(crate) fn snap(c: i32, step: u8) -> i32 {
		let s = i32::from(step);
		c.div_euclid(s) * s
	}

	/// Fill (or clear, when `v` is empty) the step³ brush cube containing
	/// `sv`, clipped to the volume. Answers whether anything changed.
	pub(crate) fn apply(&mut self, sv: [i32; 3], step: u8, v: Voxel) -> bool {
		let e = self.extent();
		let lo = sv.map(|c| Self::snap(c, step));
		let mut changed = false;
		for y in lo[1]..(lo[1] + i32::from(step)).min(e[1]) {
			for z in lo[2]..(lo[2] + i32::from(step)).min(e[2]) {
				for x in lo[0]..(lo[0] + i32::from(step)).min(e[0]) {
					let Some((p, cx, cy, cz)) = self.locate([x, y, z]) else {
						continue;
					};
					let slot = &mut self.volumes[self.active][p].0[gidx(cx, cy, cz)];
					if *slot != v {
						*slot = v;
						changed = true;
					}
				}
			}
		}
		self.dirty |= changed;
		changed
	}

	/// Compile one volume to a footprint-local box list: per cell, greedy
	/// 3D merge over identical (roles, tex) voxels — grow x, then z, then
	/// y, in a fixed scan order, so the same grid always emits the same
	/// boxes. The output is disjoint, so authored paint order is moot.
	pub(crate) fn compile(&self, volume: usize) -> Vec<DefBox> {
		let mut out = Vec::new();
		for part in 0..self.parts() {
			let (ox, oy, oz) = self.part_cell(part);
			let grid = &self.volumes[volume][part as usize];
			let mut claimed = [false; N];
			for y in 0..G {
				for z in 0..G {
					for x in 0..G {
						let i = gidx(x, y, z);
						let v = grid.0[i];
						if claimed[i] || !v.filled() {
							continue;
						}
						let free = |cx: i32, cy: i32, cz: i32| -> bool {
							let j = gidx(cx, cy, cz);
							!claimed[j] && grid.0[j] == v
						};
						let mut x1 = x + 1;
						while x1 < G && free(x1, y, z) {
							x1 += 1;
						}
						let mut z1 = z + 1;
						'z: while z1 < G {
							for cx in x..x1 {
								if !free(cx, y, z1) {
									break 'z;
								}
							}
							z1 += 1;
						}
						let mut y1 = y + 1;
						'y: while y1 < G {
							for cz in z..z1 {
								for cx in x..x1 {
									if !free(cx, y1, cz) {
										break 'y;
									}
								}
							}
							y1 += 1;
						}
						for cy in y..y1 {
							for cz in z..z1 {
								for cx in x..x1 {
									claimed[gidx(cx, cy, cz)] = true;
								}
							}
						}
						out.push(DefBox {
							sub: SubBox::new(
								[(ox * G + x) as u8, (oy * G + y) as u8, (oz * G + z) as u8],
								[
									(ox * G + x1) as u8,
									(oy * G + y1) as u8,
									(oz * G + z1) as u8,
								],
							),
							roles: v.roles,
							tex: v.tex,
						});
					}
				}
			}
		}
		out
	}

	/// The compiled document as a `ShapeDef` ready for `def::emit`.
	pub(crate) fn to_def(&self) -> ShapeDef {
		let (boxes, explicit) = match self.states {
			StateModel::Explicit(_) => (
				Vec::new(),
				(0..self.volumes.len()).map(|s| self.compile(s)).collect(),
			),
			_ => (self.compile(0), Vec::new()),
		};
		ShapeDef {
			name: self.name.clone(),
			footprint: self.footprint,
			states: self.states,
			boxes,
			explicit,
			ramp: self.ramp,
		}
	}

	/// Rebuild a document from a parsed def: rasterize every state's boxes
	/// in authored order (a later box overwrites — the loader's own overlap
	/// rule), stamping each voxel with the box's roles and textures.
	pub(crate) fn from_def(d: &ShapeDef) -> Self {
		let mut doc = ShapeDoc {
			name: d.name.clone(),
			footprint: d.footprint,
			states: d.states,
			ramp: d.ramp,
			volumes: Vec::new(),
			active: 0,
			dirty: false,
		};
		let states = match d.states {
			StateModel::Explicit(n) => n as usize,
			_ => 1,
		};
		for s in 0..states {
			let mut cells: Vec<VoxelGrid> = (0..doc.parts()).map(|_| VoxelGrid::empty()).collect();
			let list = match d.states {
				StateModel::Explicit(_) => &d.explicit[s],
				_ => &d.boxes,
			};
			for b in list {
				let v = Voxel {
					roles: b.roles,
					tex: b.tex,
				};
				for y in b.sub.min[1]..b.sub.max[1] {
					for z in b.sub.min[2]..b.sub.max[2] {
						for x in b.sub.min[0]..b.sub.max[0] {
							let sv = [i32::from(x), i32::from(y), i32::from(z)];
							let e = doc.extent();
							if sv.iter().zip(e).any(|(&c, m)| c >= m) {
								continue;
							}
							let (cx, cy, cz) = (sv[0] / G, sv[1] / G, sv[2] / G);
							let [w, _, dd] = doc.footprint;
							let part = (cx + i32::from(w) * (cz + i32::from(dd) * cy)) as usize;
							cells[part].0[gidx(sv[0] % G, sv[1] % G, sv[2] % G)] = v;
						}
					}
				}
			}
			doc.volumes.push(cells);
		}
		doc
	}

	/// Resize the footprint, keeping what fits. Refuses (false) when a
	/// shrink would drop occupied cells unless `force`.
	pub(crate) fn resize(&mut self, footprint: [u8; 3], force: bool) -> bool {
		if footprint.contains(&0)
			|| u16::from(footprint[0]) * u16::from(footprint[1]) * u16::from(footprint[2]) > 4
		{
			return false;
		}
		let old = std::mem::replace(&mut self.footprint, footprint);
		let old_parts = old[0] * old[1] * old[2];
		// Old grids keyed by their cell offset, to re-seat under the new
		// part order.
		let mut moved = false;
		let mut volumes = Vec::new();
		for vol in &self.volumes {
			let mut cells: Vec<VoxelGrid> = (0..self.parts()).map(|_| VoxelGrid::empty()).collect();
			for part in 0..old_parts {
				let [w, _, d] = old;
				let co = (
					i32::from(part % w),
					i32::from(part / (w * d)),
					i32::from((part / w) % d),
				);
				let grid = &vol[part as usize];
				let occupied = grid.0.iter().any(Voxel::filled);
				let inside = co.0 < i32::from(footprint[0])
					&& co.1 < i32::from(footprint[1])
					&& co.2 < i32::from(footprint[2]);
				if inside {
					let [w2, _, d2] = footprint;
					let new_part = co.0 + i32::from(w2) * (co.2 + i32::from(d2) * co.1);
					cells[new_part as usize] = grid.clone();
				} else if occupied {
					moved = true;
				}
			}
			volumes.push(cells);
		}
		if moved && !force {
			self.footprint = old;
			return false;
		}
		self.volumes = volumes;
		self.dirty = true;
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn brush(tex: Option<u16>) -> Voxel {
		Voxel {
			roles: Roles::ALL,
			tex: [tex; 3],
		}
	}

	#[test]
	fn place_snap_compile_round_trips() {
		let mut d = ShapeDoc::new("x");
		// A thirds brush at an unaligned point snaps to its lattice cube.
		assert!(d.apply([5, 5, 5], 4, brush(None)));
		assert!(d.occupied([4, 4, 4]) && d.occupied([7, 7, 7]));
		assert!(!d.occupied([3, 4, 4]) && !d.occupied([8, 4, 4]));
		let boxes = d.compile(0);
		assert_eq!(boxes.len(), 1);
		assert_eq!(boxes[0].sub, SubBox::new([4, 4, 4], [8, 8, 8]));
		// Removing half of it splits the box.
		assert!(d.apply([4, 4, 4], 2, Voxel::EMPTY));
		assert!(!d.occupied([5, 5, 5]));
		let boxes = d.compile(0);
		assert!(boxes.len() > 1);
		// Same grid, same boxes — the merge is deterministic.
		assert_eq!(d.compile(0), boxes);
	}

	#[test]
	fn distinct_attributes_never_merge() {
		let mut d = ShapeDoc::new("x");
		d.apply([0, 0, 0], 6, brush(None));
		d.apply([6, 0, 0], 6, brush(Some(3)));
		let boxes = d.compile(0);
		assert_eq!(boxes.len(), 2);
		assert_ne!(boxes[0].tex, boxes[1].tex);
	}

	#[test]
	fn def_round_trip_is_exact() {
		let mut d = ShapeDoc::new("chair");
		d.apply([0, 0, 0], 12, brush(None));
		d.apply([0, 6, 0], 6, Voxel::EMPTY);
		let def = d.to_def();
		let back = ShapeDoc::from_def(&def);
		assert_eq!(back.compile(0), d.compile(0));
		assert_eq!(back.footprint, d.footprint);
		// And through the text format too.
		let reparsed = digblock_core::shapes::def::parse(&digblock_core::shapes::def::emit(&def))
			.expect("editor output parses");
		assert_eq!(ShapeDoc::from_def(&reparsed).compile(0), d.compile(0));
	}

	#[test]
	fn built_in_files_survive_a_doc_round_trip() {
		// Load every shipped .shape into the grid and compile back: the box
		// DECOMPOSITION may differ from the hand-authored one, but the
		// rasterized volume per (roles, tex) must be identical.
		use digblock_core::shapes::raster_volume;
		for &(name, src) in digblock_core::shapes::def::ASSETS {
			let parsed = digblock_core::shapes::def::parse(src).unwrap();
			let doc = ShapeDoc::from_def(&parsed);
			let states = doc.volumes.len();
			for s in 0..states {
				let original = match parsed.states {
					StateModel::Explicit(_) => &parsed.explicit[s],
					_ => &parsed.boxes,
				};
				let compiled = doc.compile(s);
				// Compare per-cell occupancy of the two lists.
				let back = ShapeDoc::from_def(&ShapeDef {
					boxes: compiled.clone(),
					explicit: Vec::new(),
					states: StateModel::None_,
					..parsed.clone()
				});
				let orig_doc = ShapeDoc::from_def(&ShapeDef {
					boxes: original.clone(),
					explicit: Vec::new(),
					states: StateModel::None_,
					..parsed.clone()
				});
				for part in 0..doc.parts() as usize {
					assert!(
						back.volumes[0][part] == orig_doc.volumes[0][part],
						"{name} state {s} cell {part} drifted through the grid"
					);
				}
				let _ = raster_volume; // volume identity is implied by grid equality
			}
		}
	}

	#[test]
	fn resize_refuses_to_drop_material_unless_forced() {
		let mut d = ShapeDoc::new("x");
		assert!(d.resize([1, 2, 1], false));
		d.active = 0;
		// Material in the upper cell (part 1).
		assert!(d.apply([0, 14, 0], 2, brush(None)));
		assert!(!d.resize([1, 1, 1], false), "occupied cell refused");
		assert_eq!(d.footprint, [1, 2, 1]);
		assert!(d.resize([1, 1, 1], true), "forced shrink");
		assert_eq!(d.parts(), 1);
		assert!(!d.resize([2, 2, 2], false), "over the 4-part cap");
	}
}
