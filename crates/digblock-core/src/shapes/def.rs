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
//! states none|facing|door|explicit <n>
//! ramp                                   # optional; stairs' diagonal physics
//! box <roles> x0 y0 z0 x1 y1 z1 [top=N] [side=N] [bottom=N] [all=N]
//! state <k>                              # explicit model: section for state k
//! ```
//!
//! Coordinates are footprint-local twelfths (min inclusive, max exclusive),
//! authored in the canonical +x facing. Roles are `all` or `+`-joined subsets
//! of visual/collision/light/interaction. Textures are named by their
//! `block::tex` constant, never by index. Malformed input is an `Err` here and
//! fatal in every loader — never a silent default. `emit` is the editor's
//! writer; parse(emit(x)) is a fixpoint (comments are not round-tripped).

use super::SubBox;
use crate::block::{ShapeKind, tex};
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

/// How a shape's state nibble maps to variants. `Door` derives all 16
/// (facing × open × part) from one authored closed pose; `Explicit` carries
/// one authored section per state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StateModel {
	None_,
	Facing,
	Door,
	Explicit(u8),
}

/// A parsed `.shape` file: everything the registry bakes a family from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShapeDef {
	pub name: String,
	pub footprint: [u8; 3],
	pub states: StateModel,
	/// Canonical boxes (models other than `Explicit`).
	pub boxes: Vec<DefBox>,
	/// `Explicit` model: one authored box list per state, in order.
	pub explicit: Vec<Vec<DefBox>>,
	pub ramp: bool,
}

impl ShapeDef {
	/// The authored boxes for one state under this def's model (the
	/// canonical pose for derived models; the state's own section for
	/// `Explicit`).
	pub fn state_boxes(&self, state: u8) -> &[DefBox] {
		match self.states {
			StateModel::Explicit(_) => &self.explicit[state as usize],
			_ => &self.boxes,
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
					_ => return Err("usage: states none|facing|door|explicit <n>".into()),
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
				let [k] = args else {
					return Err("usage: state <k>".into());
				};
				let k: u8 = k.parse().map_err(|_| "bad state index")?;
				if usize::from(k) != self.explicit.len() || k >= n {
					return Err(format!(
						"state sections must run 0..{n} in order (expected {})",
						self.explicit.len()
					));
				}
				if self.explicit.last().is_some_and(Vec::is_empty) {
					return Err("previous state section has no boxes".into());
				}
				self.explicit.push(Vec::new());
				Ok(())
			}
			"box" => self.push_box(args),
			_ => Err(format!("unknown directive `{word}`")),
		}
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
			StateModel::Explicit(_) => self
				.explicit
				.last_mut()
				.ok_or("box before the first state section")?
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
			StateModel::Explicit(n) => {
				if self.explicit.len() != usize::from(n) {
					return Err(format!(
						"expected {n} state sections, found {}",
						self.explicit.len()
					));
				}
				if self.explicit.last().is_some_and(Vec::is_empty) {
					return Err("last state section has no boxes".into());
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
			ramp: self.ramp,
		})
	}
}

fn coord(tok: &str) -> Result<u8, String> {
	tok.parse().map_err(|_| format!("bad number `{tok}`"))
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
	}
	if def.ramp {
		writeln!(s, "ramp").unwrap();
	}
	match def.states {
		StateModel::Explicit(_) => {
			for (k, list) in def.explicit.iter().enumerate() {
				writeln!(s, "state {k}").unwrap();
				for b in list {
					emit_box(&mut s, b);
				}
			}
		}
		_ => {
			for b in &def.boxes {
				emit_box(&mut s, b);
			}
		}
	}
	s
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

/// (asset name, embedded source) of every shipped `.shape` — grows as the
/// hardcoded families are ported until all seven `ShapeKind`s live here.
pub const ASSETS: &[(&str, &str)] = &[
	("bed", include_str!("../../../../assets/shapes/bed.shape")),
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
		"stairs" => ShapeKind::Stairs,
		"door" => ShapeKind::Door,
		"slab" => ShapeKind::Slab,
		"torch" => ShapeKind::Torch,
		"table" => ShapeKind::Table,
		"chair" => ShapeKind::Chair,
		"bed" => ShapeKind::Bed,
		_ => return None,
	})
}

/// The state model the code-side nibble layout defines for a kind; a file
/// declaring anything else is refused at load.
fn kind_model(kind: ShapeKind) -> StateModel {
	match kind {
		ShapeKind::Stairs | ShapeKind::Chair | ShapeKind::Bed => StateModel::Facing,
		ShapeKind::Door => StateModel::Door,
		ShapeKind::Slab => StateModel::Explicit(4),
		ShapeKind::Torch | ShapeKind::Table => StateModel::None_,
	}
}

/// The footprint the engine's machinery expects for a kind — the state
/// nibble layout is the one authority.
fn kind_footprint(kind: ShapeKind) -> [u8; 3] {
	super::state_layout(kind).footprint
}

static DEFS: LazyLock<Vec<(ShapeKind, ShapeDef)>> = LazyLock::new(|| {
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
			(kind, d)
		})
		.collect()
});

/// The loaded definition for a ported kind. Panics for a kind not yet in the
/// roster — the registry only asks for kinds it was rewired to.
pub(crate) fn of(kind: ShapeKind) -> &'static ShapeDef {
	DEFS.iter()
		.find(|&&(k, _)| k == kind)
		.map(|(_, d)| d)
		.unwrap_or_else(|| panic!("no shape asset for {kind:?}"))
}
