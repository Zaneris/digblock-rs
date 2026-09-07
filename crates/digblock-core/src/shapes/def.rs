//! The `.shape` asset format: sub-block geometry as committed plain text,
//! embedded at compile time and parsed into [`ShapeDef`]s the registry bakes
//! from — replacing the old hardcoded box consts. Grammar, in directive order
//! (`#` comments, whitespace-separated tokens):
//!
//! ```text
//! version 1
//! shape <name>
//! grid 12
//! footprint W H D                        # optional, default 1 1 1
//! states none|facing|door|explicit <n>|forms <n>
//! ramp                                   # optional; stairs' diagonal physics
//! box <roles> x0 y0 z0 x1 y1 z1 [top=N] [side=N] [bottom=N] [all=N]
//! tri  x y z  x y z  x y z  dir=+x [tex=N]        # visual-only sloped face
//! quad x y z  x y z  x y z  x y z  dir=-y [tex=N] # ditto, four corners around
//! state <k>                              # explicit model: section for state k
//! form <k>                               # forms model: section for form k
//! ```
//!
//! Coordinates are footprint-local twelfths (min inclusive, max exclusive),
//! authored in the canonical +x facing. Roles are `all` or `+`-joined subsets
//! of visual/collision/light/interaction. Textures are named by their
//! `block::tex` constant, never by index. Polygons draw as authored (any
//! plane, lit and textured as the axis face `dir` names) and feed nothing but
//! the look; boxes still carry what a body, a ray and light meet. Malformed
//! input is an `Err` here and fatal in every loader — never a silent default.
//! `emit` is the editor's writer; parse(emit(x)) is a fixpoint (comments are
//! not round-tripped).

use super::SubBox;
use crate::block::{ShapeKind, ShapeStyle, tex};
use crate::dir::Dir;
use std::sync::LazyLock;

/// Which of a variant's derived lists a box feeds. Non-empty by construction
/// (the parser refuses a role-less box).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Roles {
	pub visual: bool,
	pub collision: bool,
	pub light: bool,
	pub interaction: bool,
}

impl Roles {
	pub const ALL: Roles = Roles {
		visual: true,
		collision: true,
		light: true,
		interaction: true,
	};
	const NONE: Roles = Roles {
		visual: false,
		collision: false,
		light: false,
		interaction: false,
	};
}

/// One authored box: geometry, the lists it feeds, and per-face texture
/// overrides (top/side/bottom layer, `None` = the block row's default).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefBox {
	pub sub: SubBox,
	pub roles: Roles,
	pub tex: [Option<u16>; 3],
}

/// One authored polygon: three or four corners in perimeter order, the axis
/// face it is lit and textured as, and a texture override (`None` = the
/// block row's layer for that face). Visual only; winding is fixed at bake.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefPoly {
	pub pts: Vec<[u8; 3]>,
	pub dir: Dir,
	pub tex: Option<u16>,
}

/// How a shape's state nibble maps to variants. `Door` derives all 16
/// (facing × open × part) from one authored closed pose; `Explicit` carries
/// one authored section per state; `Forms` carries one section per FORM,
/// each turned by the facing in the nibble's low two bits (state = form
/// × 4 + facing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StateModel {
	None_,
	Facing,
	Door,
	Explicit(u8),
	Forms(u8),
}

impl StateModel {
	/// Authored sections this model carries (one for the derived models).
	pub fn sections(self) -> usize {
		match self {
			StateModel::Explicit(n) | StateModel::Forms(n) => usize::from(n),
			_ => 1,
		}
	}

	/// Which authored section a state reads.
	pub fn section_of(self, state: u8) -> usize {
		match self {
			StateModel::Explicit(_) => usize::from(state),
			StateModel::Forms(_) => usize::from(state >> 2),
			_ => 0,
		}
	}
}

/// A parsed `.shape` file: everything the registry bakes a family from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShapeDef {
	pub name: String,
	pub footprint: [u8; 3],
	pub states: StateModel,
	/// Canonical boxes (models other than `Explicit` and `Forms`).
	pub boxes: Vec<DefBox>,
	/// `Explicit` / `Forms` models: one authored box list per section.
	pub explicit: Vec<Vec<DefBox>>,
	/// Canonical polygons, alongside `boxes`.
	pub polys: Vec<DefPoly>,
	/// One polygon list per section, alongside `explicit`.
	pub explicit_polys: Vec<Vec<DefPoly>>,
	pub ramp: bool,
}

impl ShapeDef {
	/// The authored boxes for one state under this def's model (the
	/// canonical pose for derived models; the state's own section for
	/// `Explicit`, its form's for `Forms`).
	pub fn state_boxes(&self, state: u8) -> &[DefBox] {
		match self.states {
			StateModel::Explicit(_) | StateModel::Forms(_) => {
				&self.explicit[self.states.section_of(state)]
			}
			_ => &self.boxes,
		}
	}

	/// The authored polygons for one state, chosen like [`Self::state_boxes`].
	pub fn state_polys(&self, state: u8) -> &[DefPoly] {
		match self.states {
			StateModel::Explicit(_) | StateModel::Forms(_) => {
				&self.explicit_polys[self.states.section_of(state)]
			}
			_ => &self.polys,
		}
	}
}

/// Most cells one object may wear (footprint W*H*D cap): what the state
/// nibble's spare bits can address alongside a facing.
pub const MAX_PARTS: u8 = 4;

/// Parse one `.shape` source. Errors carry the 1-based line number; every
/// loader treats them as fatal.
pub fn parse(src: &str) -> Result<ShapeDef, String> {
	let mut p = Parser::default();
	for (i, raw) in src.lines().enumerate() {
		let line = raw.split('#').next().unwrap_or("");
		let toks: Vec<&str> = line.split_whitespace().collect();
		if toks.is_empty() {
			continue;
		}
		p.directive(&toks)
			.map_err(|e| format!("line {}: {e}", i + 1))?;
	}
	p.finish()
}

#[derive(Default)]
struct Parser {
	version: bool,
	name: Option<String>,
	grid: bool,
	footprint: Option<[u8; 3]>,
	states: Option<StateModel>,
	ramp: bool,
	boxes: Vec<DefBox>,
	explicit: Vec<Vec<DefBox>>,
	polys: Vec<DefPoly>,
	explicit_polys: Vec<Vec<DefPoly>>,
}

impl Parser {
	fn directive(&mut self, toks: &[&str]) -> Result<(), String> {
		let (word, args) = (toks[0], &toks[1..]);
		if word != "version" && !self.version {
			return Err("the first directive must be `version 1`".into());
		}
		match word {
			"version" => match args {
				["1"] if !self.version => {
					self.version = true;
					Ok(())
				}
				["1"] => Err("duplicate version".into()),
				_ => Err("unknown version (this build reads version 1)".into()),
			},
			"shape" => match args {
				[n] if self.name.is_none() => {
					self.name = Some((*n).into());
					Ok(())
				}
				[_] => Err("duplicate shape".into()),
				_ => Err("usage: shape <name>".into()),
			},
			"grid" => match args {
				["12"] if !self.grid => {
					self.grid = true;
					Ok(())
				}
				["12"] => Err("duplicate grid".into()),
				_ => Err("grid must be 12".into()),
			},
			"footprint" => {
				if self.footprint.is_some() {
					return Err("duplicate footprint".into());
				}
				if self.states.is_some() {
					return Err("footprint must come before states".into());
				}
				let [w, h, d] = args else {
					return Err("usage: footprint W H D".into());
				};
				let f = [coord(w)?, coord(h)?, coord(d)?];
				if f.contains(&0) {
					return Err("footprint dimensions start at 1".into());
				}
				let parts = u16::from(f[0]) * u16::from(f[1]) * u16::from(f[2]);
				if parts > u16::from(MAX_PARTS) {
					return Err(format!("footprint exceeds {MAX_PARTS} cells"));
				}
				self.footprint = Some(f);
				Ok(())
			}
			"states" => {
				if self.states.is_some() {
					return Err("duplicate states".into());
				}
				self.states = Some(match args {
					["none"] => StateModel::None_,
					["facing"] => StateModel::Facing,
					["door"] => StateModel::Door,
					["explicit", n] => {
						let n: u8 = n.parse().map_err(|_| "bad state count")?;
						if n == 0 || n > 16 {
							return Err("explicit state count must be 1..=16".into());
						}
						StateModel::Explicit(n)
					}
					["forms", n] => {
						let n: u8 = n.parse().map_err(|_| "bad form count")?;
						if n == 0 || n > 4 {
							return Err("form count must be 1..=4 (four facings each)".into());
						}
						StateModel::Forms(n)
					}
					_ => {
						return Err("usage: states none|facing|door|explicit <n>|forms <n>".into());
					}
				});
				Ok(())
			}
			"ramp" => {
				if !args.is_empty() {
					return Err("ramp takes no arguments".into());
				}
				if self.ramp {
					return Err("duplicate ramp".into());
				}
				self.ramp = true;
				Ok(())
			}
			"state" => {
				let Some(StateModel::Explicit(n)) = self.states else {
					return Err("state sections need `states explicit <n>`".into());
				};
				self.open_section("state", n, args)
			}
			"form" => {
				let Some(StateModel::Forms(n)) = self.states else {
					return Err("form sections need `states forms <n>`".into());
				};
				self.open_section("form", n, args)
			}
			"box" => self.push_box(args),
			"tri" => self.push_poly(3, args),
			"quad" => self.push_poly(4, args),
			_ => Err(format!("unknown directive `{word}`")),
		}
	}

	/// Start section k of a sectioned model (`state k` / `form k`): they
	/// run 0..n in order, and a section never closes empty of boxes.
	fn open_section(&mut self, word: &str, n: u8, args: &[&str]) -> Result<(), String> {
		let [k] = args else {
			return Err(format!("usage: {word} <k>"));
		};
		let k: u8 = k.parse().map_err(|_| format!("bad {word} index"))?;
		if usize::from(k) != self.explicit.len() || k >= n {
			return Err(format!(
				"{word} sections must run 0..{n} in order (expected {})",
				self.explicit.len()
			));
		}
		if self.explicit.last().is_some_and(Vec::is_empty) {
			return Err(format!("previous {word} section has no boxes"));
		}
		self.explicit.push(Vec::new());
		self.explicit_polys.push(Vec::new());
		Ok(())
	}

	fn push_poly(&mut self, n: usize, args: &[&str]) -> Result<(), String> {
		let Some(states) = self.states else {
			return Err("polygon lines must come after states".into());
		};
		if states == StateModel::Door {
			return Err("the door model swings boxes only; polygons are not supported".into());
		}
		let word = if n == 3 { "tri" } else { "quad" };
		if args.len() < n * 3 + 1 {
			return Err(format!(
				"usage: {word} {}dir=±x|±y|±z [tex=NAME]",
				"x y z  ".repeat(n)
			));
		}
		let mut pts = Vec::with_capacity(n);
		for i in 0..n {
			pts.push([
				coord(args[i * 3])?,
				coord(args[i * 3 + 1])?,
				coord(args[i * 3 + 2])?,
			]);
		}
		let fp = self.footprint.unwrap_or([1, 1, 1]);
		poly_cell(fp, &pts)?;
		let mut dir = None;
		let mut tex_over = None;
		for t in &args[n * 3..] {
			let (key, value) = t
				.split_once('=')
				.ok_or_else(|| format!("expected dir=… or tex=NAME, got `{t}`"))?;
			match key {
				"dir" => dir = Some(parse_dir(value)?),
				"tex" => {
					tex_over = Some(
						tex::layer_by_name(value)
							.ok_or_else(|| format!("unknown texture name `{value}`"))?,
					);
				}
				_ => return Err(format!("unknown polygon key `{key}` (dir/tex)")),
			}
		}
		let dir = dir.ok_or("polygon needs dir=±x|±y|±z")?;
		super::poly_check(&pts).map_err(|e| format!("{word}: {e}"))?;
		let p = DefPoly {
			pts,
			dir,
			tex: tex_over,
		};
		match states {
			StateModel::Explicit(_) | StateModel::Forms(_) => self
				.explicit_polys
				.last_mut()
				.ok_or("polygon before the first section")?
				.push(p),
			_ => self.polys.push(p),
		}
		Ok(())
	}

	fn push_box(&mut self, args: &[&str]) -> Result<(), String> {
		let Some(states) = self.states else {
			return Err("box lines must come after states".into());
		};
		if args.len() < 7 {
			return Err("usage: box <roles> x0 y0 z0 x1 y1 z1 [face=NAME ...]".into());
		}
		let roles = parse_roles(args[0])?;
		let c: [u8; 6] = [
			coord(args[1])?,
			coord(args[2])?,
			coord(args[3])?,
			coord(args[4])?,
			coord(args[5])?,
			coord(args[6])?,
		];
		let fp = self.footprint.unwrap_or([1, 1, 1]);
		for a in 0..3 {
			if c[a] >= c[a + 3] {
				return Err("box min must be strictly below max on every axis".into());
			}
			if c[a + 3] > fp[a] * super::GRID {
				return Err("box extends past the footprint".into());
			}
		}
		let mut tex_over = [None; 3];
		for t in &args[7..] {
			let (face, name) = t
				.split_once('=')
				.ok_or_else(|| format!("expected face=NAME, got `{t}`"))?;
			let layer =
				tex::layer_by_name(name).ok_or_else(|| format!("unknown texture name `{name}`"))?;
			match face {
				"top" => tex_over[0] = Some(layer),
				"side" => tex_over[1] = Some(layer),
				"bottom" => tex_over[2] = Some(layer),
				"all" => tex_over = [Some(layer); 3],
				_ => return Err(format!("unknown face `{face}` (top/side/bottom/all)")),
			}
		}
		let b = DefBox {
			sub: SubBox::new([c[0], c[1], c[2]], [c[3], c[4], c[5]]),
			roles,
			tex: tex_over,
		};
		match states {
			StateModel::Explicit(_) | StateModel::Forms(_) => self
				.explicit
				.last_mut()
				.ok_or("box before the first section")?
				.push(b),
			_ => self.boxes.push(b),
		}
		Ok(())
	}

	fn finish(self) -> Result<ShapeDef, String> {
		let name = self.name.ok_or("missing shape directive")?;
		if !self.grid {
			return Err("missing grid directive".into());
		}
		let states = self.states.ok_or("missing states directive")?;
		match states {
			StateModel::Explicit(n) | StateModel::Forms(n) => {
				if self.explicit.len() != usize::from(n) {
					return Err(format!(
						"expected {n} sections, found {}",
						self.explicit.len()
					));
				}
				if self.explicit.last().is_some_and(Vec::is_empty) {
					return Err("last section has no boxes".into());
				}
			}
			_ if self.boxes.is_empty() => return Err("no boxes".into()),
			// The swing envelope: every door box must live inside the leaf's
			// pivot quarter on x so its open pose stays inside the cell.
			StateModel::Door if self.boxes.iter().any(|b| b.sub.max[0] > 4) => {
				return Err("door box outside the swing envelope (max x is 4)".into());
			}
			_ => {}
		}
		Ok(ShapeDef {
			name,
			footprint: self.footprint.unwrap_or([1, 1, 1]),
			states,
			boxes: self.boxes,
			explicit: self.explicit,
			polys: self.polys,
			explicit_polys: self.explicit_polys,
			ramp: self.ramp,
		})
	}
}

fn coord(tok: &str) -> Result<u8, String> {
	tok.parse().map_err(|_| format!("bad number `{tok}`"))
}

/// The footprint cell a polygon lies in: on every axis, the cell range each
/// corner admits (a corner on a cell boundary admits both sides), intersected
/// over the corners. A polygon straddling two cells is refused — the bake
/// slices a footprint per cell and cannot cut a polygon in two. A polygon
/// lying wholly on a shared boundary plane goes to the lower cell.
pub(super) fn poly_cell(fp: [u8; 3], pts: &[[u8; 3]]) -> Result<[u8; 3], String> {
	let g = super::GRID;
	let mut cell = [0u8; 3];
	for a in 0..3 {
		let (mut lo, mut hi) = (0u8, fp[a] - 1);
		for p in pts {
			if p[a] > fp[a] * g {
				return Err("polygon corner outside the footprint".into());
			}
			let c_hi = (p[a] / g).min(fp[a] - 1);
			let c_lo = if p[a] % g == 0 && p[a] > 0 {
				p[a] / g - 1
			} else {
				c_hi
			};
			lo = lo.max(c_lo);
			hi = hi.min(c_hi);
		}
		if lo > hi {
			return Err("polygon straddles footprint cells".into());
		}
		cell[a] = lo;
	}
	Ok(cell)
}

/// Axis-face names as the grammar spells them: signed axes, never the
/// compass words `Dir` carries (West is +x there, a trap in a text file).
const DIR_NAMES: [(&str, Dir); 6] = [
	("+x", Dir::West),
	("-x", Dir::East),
	("+y", Dir::Top),
	("-y", Dir::Bottom),
	("+z", Dir::North),
	("-z", Dir::South),
];

fn parse_dir(tok: &str) -> Result<Dir, String> {
	DIR_NAMES
		.iter()
		.find(|(n, _)| *n == tok)
		.map(|&(_, d)| d)
		.ok_or_else(|| format!("bad dir `{tok}` (+x -x +y -y +z -z)"))
}

fn dir_name(d: Dir) -> &'static str {
	DIR_NAMES
		.iter()
		.find(|(_, v)| *v == d)
		.map(|&(n, _)| n)
		.expect("every Dir has a name")
}

fn parse_roles(tok: &str) -> Result<Roles, String> {
	if tok == "all" {
		return Ok(Roles::ALL);
	}
	let mut r = Roles::NONE;
	for part in tok.split('+') {
		match part {
			"visual" => r.visual = true,
			"collision" => r.collision = true,
			"light" => r.light = true,
			"interaction" => r.interaction = true,
			_ => return Err(format!("unknown role `{part}`")),
		}
	}
	if r == Roles::NONE {
		return Err("empty roles".into());
	}
	Ok(r)
}

/// Serialize a def back to `.shape` text — the editor's writer. Deterministic
/// for a given def; box order is preserved (it is the overlap paint order).
pub fn emit(def: &ShapeDef) -> String {
	use std::fmt::Write;
	let mut s = String::from("# generated by the shape editor\n");
	writeln!(s, "version 1").unwrap();
	writeln!(s, "shape {}", def.name).unwrap();
	writeln!(s, "grid {}", super::GRID).unwrap();
	if def.footprint != [1, 1, 1] {
		let [w, h, d] = def.footprint;
		writeln!(s, "footprint {w} {h} {d}").unwrap();
	}
	match def.states {
		StateModel::None_ => writeln!(s, "states none").unwrap(),
		StateModel::Facing => writeln!(s, "states facing").unwrap(),
		StateModel::Door => writeln!(s, "states door").unwrap(),
		StateModel::Explicit(n) => writeln!(s, "states explicit {n}").unwrap(),
		StateModel::Forms(n) => writeln!(s, "states forms {n}").unwrap(),
	}
	if def.ramp {
		writeln!(s, "ramp").unwrap();
	}
	match def.states {
		StateModel::Explicit(_) | StateModel::Forms(_) => {
			let word = if matches!(def.states, StateModel::Forms(_)) {
				"form"
			} else {
				"state"
			};
			for (k, list) in def.explicit.iter().enumerate() {
				writeln!(s, "{word} {k}").unwrap();
				for b in list {
					emit_box(&mut s, b);
				}
				for p in def.explicit_polys.get(k).into_iter().flatten() {
					emit_poly(&mut s, p);
				}
			}
		}
		_ => {
			for b in &def.boxes {
				emit_box(&mut s, b);
			}
			for p in &def.polys {
				emit_poly(&mut s, p);
			}
		}
	}
	s
}

fn emit_poly(s: &mut String, p: &DefPoly) {
	use std::fmt::Write;
	s.push_str(if p.pts.len() == 3 { "tri " } else { "quad" });
	for c in &p.pts {
		write!(s, "  {} {} {}", c[0], c[1], c[2]).unwrap();
	}
	write!(s, "  dir={}", dir_name(p.dir)).unwrap();
	if let Some(layer) = p.tex {
		write!(s, "  tex={}", layer_name(layer)).unwrap();
	}
	s.push('\n');
}

fn emit_box(s: &mut String, b: &DefBox) {
	use std::fmt::Write;
	let r = b.roles;
	if r == Roles::ALL {
		s.push_str("box all");
	} else {
		s.push_str("box ");
		let mut sep = "";
		for (on, name) in [
			(r.visual, "visual"),
			(r.collision, "collision"),
			(r.light, "light"),
			(r.interaction, "interaction"),
		] {
			if on {
				write!(s, "{sep}{name}").unwrap();
				sep = "+";
			}
		}
	}
	let (lo, hi) = (b.sub.min, b.sub.max);
	write!(
		s,
		"  {} {} {}  {} {} {}",
		lo[0], lo[1], lo[2], hi[0], hi[1], hi[2]
	)
	.unwrap();
	if b.tex[0].is_some() && b.tex == [b.tex[0]; 3] {
		write!(s, "  all={}", layer_name(b.tex[0].unwrap())).unwrap();
	} else {
		for (i, face) in ["top", "side", "bottom"].iter().enumerate() {
			if let Some(layer) = b.tex[i] {
				write!(s, "  {face}={}", layer_name(layer)).unwrap();
			}
		}
	}
	s.push('\n');
}

/// The canonical name of a resolved layer (inverse of `tex::layer_by_name`;
/// resolved layers always came from a name, so absence is a bug).
fn layer_name(layer: u16) -> &'static str {
	tex::NAMED
		.iter()
		.find(|&&(_, v)| v == layer)
		.map(|&(n, _)| n)
		.expect("resolved layer has a name")
}

// --- the built-in roster ----------------------------------------------

/// (asset name, embedded source) of every shipped `.shape`: one per kind,
/// plus one per alternate look (`<kind>_<style>`, see [`asset_name`]).
pub const ASSETS: &[(&str, &str)] = &[
	("bed", include_str!("../../../../assets/shapes/bed.shape")),
	(
		"canopy",
		include_str!("../../../../assets/shapes/canopy.shape"),
	),
	(
		"chair",
		include_str!("../../../../assets/shapes/chair.shape"),
	),
	("door", include_str!("../../../../assets/shapes/door.shape")),
	("slab", include_str!("../../../../assets/shapes/slab.shape")),
	(
		"stairs",
		include_str!("../../../../assets/shapes/stairs.shape"),
	),
	(
		"stairs_lumber",
		include_str!("../../../../assets/shapes/stairs_lumber.shape"),
	),
	(
		"stairs_stone",
		include_str!("../../../../assets/shapes/stairs_stone.shape"),
	),
	(
		"table",
		include_str!("../../../../assets/shapes/table.shape"),
	),
	(
		"torch",
		include_str!("../../../../assets/shapes/torch.shape"),
	),
];

/// The kind an asset name binds to; the loader refuses names it doesn't know.
pub fn kind_by_name(name: &str) -> Option<ShapeKind> {
	Some(match name {
		"stairs" | "stairs_lumber" | "stairs_stone" => ShapeKind::Stairs,
		"door" => ShapeKind::Door,
		"slab" => ShapeKind::Slab,
		"torch" => ShapeKind::Torch,
		"table" => ShapeKind::Table,
		"chair" => ShapeKind::Chair,
		"bed" => ShapeKind::Bed,
		"canopy" => ShapeKind::Canopy,
		_ => return None,
	})
}

/// A kind's canonical asset name: the file its `Plain` look bakes from.
fn kind_name(kind: ShapeKind) -> &'static str {
	match kind {
		ShapeKind::Stairs => "stairs",
		ShapeKind::Door => "door",
		ShapeKind::Slab => "slab",
		ShapeKind::Torch => "torch",
		ShapeKind::Table => "table",
		ShapeKind::Chair => "chair",
		ShapeKind::Bed => "bed",
		ShapeKind::Canopy => "canopy",
	}
}

/// The asset a (kind, look) pair bakes from: the kind's own file for
/// `Plain`, `<kind>_<style>` for an alternate look. The ONE place the
/// enum becomes a file name; `of` panics if the roster lacks it.
pub fn asset_name(kind: ShapeKind, style: ShapeStyle) -> String {
	let kind = kind_name(kind);
	match style {
		ShapeStyle::Plain => kind.to_string(),
		ShapeStyle::Lumber => format!("{kind}_lumber"),
		ShapeStyle::Stone => format!("{kind}_stone"),
	}
}

/// The state model the code-side nibble layout defines for a kind; a file
/// declaring anything else is refused at load.
fn kind_model(kind: ShapeKind) -> StateModel {
	match kind {
		ShapeKind::Stairs | ShapeKind::Chair | ShapeKind::Bed => StateModel::Facing,
		ShapeKind::Door => StateModel::Door,
		ShapeKind::Slab => StateModel::Explicit(4),
		ShapeKind::Torch | ShapeKind::Table => StateModel::None_,
		ShapeKind::Canopy => StateModel::Forms(super::CANOPY_FORMS),
	}
}

/// The footprint the engine's machinery expects for a kind — the state
/// nibble layout is the one authority.
fn kind_footprint(kind: ShapeKind) -> [u8; 3] {
	super::state_layout(kind).footprint
}

static DEFS: LazyLock<Vec<(&'static str, ShapeDef)>> = LazyLock::new(|| {
	ASSETS
		.iter()
		.map(|&(name, src)| {
			let fail = |m: &str| -> ! { panic!("shape asset {name}: {m}") };
			let d = parse(src).unwrap_or_else(|e| fail(&e));
			let kind = kind_by_name(name).unwrap_or_else(|| fail("not a ShapeKind asset name"));
			if d.name != name {
				fail("shape directive disagrees with the asset name");
			}
			if d.states != kind_model(kind) {
				fail("states model disagrees with the kind's nibble layout");
			}
			if d.footprint != kind_footprint(kind) {
				fail("footprint disagrees with the kind's machinery");
			}
			if d.ramp != (kind == ShapeKind::Stairs) {
				fail("ramp is stairs' physics contract, exactly");
			}
			(name, d)
		})
		.collect()
});

/// The loaded definition for a kind in one of its looks. Panics for a pair
/// the roster has no file for — a styled block row names a file that must
/// exist, and `every_shape_asset_parses_and_bakes` asks for every row's.
pub(crate) fn of(kind: ShapeKind, style: ShapeStyle) -> &'static ShapeDef {
	let name = asset_name(kind, style);
	DEFS.iter()
		.find(|&&(n, _)| n == name)
		.map(|(_, d)| d)
		.unwrap_or_else(|| panic!("no shape asset {name} for {kind:?} in the {style:?} look"))
}
