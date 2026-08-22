//! The capture scenes and A/B levers, parsed in ONE place for both
//! launchers: desktop reads each knob from `DIG_*` in the environment,
//! the web page from `?name=` on its URL, and [`apply`] decides what the
//! value means. A knob that exists on one platform and not the other is
//! the thing this module makes impossible — the table is the roster, and
//! `every_knob_is_read` keeps a row from being added without an arm.
//!
//! A malformed value is an ERROR, never a silent default: a sweep arm that
//! quietly ran the shipped scene is the worst outcome of a typo, and the
//! launcher turns the error into its own loud exit (exit 2; a panic the
//! console shows). A flag knob is ON by presence — `DIG_VAULT=0` and
//! `?vault=0` both build the vault — because a modifier that silently
//! built nothing photographs open terrain as a finding.

use super::*;

/// Whether a knob is a switch or carries a value. The harness that
/// drives the web build reads this to decide how to spell a flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
	Flag,
	Value,
}

/// One knob: its query spelling, its environment spelling, and its shape.
#[derive(Clone, Copy, Debug)]
pub struct Knob {
	/// The web's `?name` and the harness's `--name`.
	pub name: &'static str,
	/// The desktop's environment variable.
	pub env: &'static str,
	pub kind: Kind,
	/// One line for `--help` and the harness's usage text.
	pub help: &'static str,
}

const fn flag(name: &'static str, env: &'static str, help: &'static str) -> Knob {
	Knob {
		name,
		env,
		kind: Kind::Flag,
		help,
	}
}

const fn value(name: &'static str, env: &'static str, help: &'static str) -> Knob {
	Knob {
		name,
		env,
		kind: Kind::Value,
		help,
	}
}

pub const PLAYERBOX: Knob = value(
	"playerbox",
	"DIG_PLAYERBOX",
	"a fake remote holding <item> (dirt|torch|stairs|slab|door|bucket|bucket-full|siphon|pick|sigil|empty)",
);
pub const CONFETTI: Knob = flag("confetti", "DIG_CONFETTI", "the milestone confetti burst");
pub const SHAPES: Knob = flag(
	"shapes",
	"DIG_SHAPES",
	"every shaped block laid out in a row",
);
pub const DOORS: Knob = flag("doors", "DIG_DOORS", "doors in every state");
pub const WARD: Knob = value(
	"ward",
	"DIG_WARD",
	"a ward of <radius> closed about spawn, with no cast behind it",
);
pub const VAULT: Knob = flag("vault", "DIG_VAULT", "a sealed stone vault with a torch");
pub const VAULT_DARK: Knob = flag(
	"vaultdark",
	"DIG_VAULT_DARK",
	"the vault with no torch on its floor (implies vault)",
);
pub const ROOF: Knob = flag("roof", "DIG_ROOF", "a roofed room under the sky dome");
pub const TUNNEL: Knob = flag(
	"tunnel",
	"DIG_TUNNEL",
	"a sealed bore past the cascades' coverage",
);
pub const DIVE: Knob = flag("dive", "DIG_DIVE", "submerged in the sea");
pub const RIVER: Knob = flag(
	"river",
	"DIG_RIVER",
	"a flooded basin, the shaded-water pose",
);
pub const TORCHES_WALL: Knob = flag(
	"torcheswall",
	"DIG_TORCHES_WALL",
	"the courtyard's torches on a wall",
);
pub const ORE: Knob = value(
	"ore",
	"DIG_ORE",
	"a chamber around the nearest deposit of <tier 1..=4>, its level granted",
);
pub const NODE: Knob = value(
	"node",
	"DIG_NODE",
	"pose at the nearest <heartwood|leystone|springstone>, its reveal granted",
);
pub const MINING: Knob = value("mining", "DIG_MINING", "start at Mining <level>");
pub const CRACK: Knob = value("crack", "DIG_CRACK", "pin the crack decal at <percent>");
pub const TORCHES: Knob = value("torches", "DIG_TORCHES", "the courtyard lit by <n> torches");
pub const TORCHES_FAR: Knob = value(
	"torchesfar",
	"DIG_TORCHES_FAR",
	"<n> torches padding the light list out of view (rides torches)",
);
pub const GI_ITERS: Knob = value(
	"giiters",
	"DIG_GI_ITERS",
	"the resolve's ray budgets <gather>,<sun>",
);
pub const STUB: Knob = value(
	"stub",
	"DIG_STUB",
	"shading terms to remove, comma-separated",
);
pub const GI: Knob = value(
	"gi",
	"DIG_GI",
	"paint the voxel window (window) or the probe field (probes)",
);
pub const SCREEN: Knob = value(
	"screen",
	"DIG_SCREEN",
	"open a UI screen or hold an item at startup",
);
pub const CAST: Knob = value(
	"cast",
	"DIG_CAST",
	"pose the cast channel's aura at <percent>",
);
pub const SIPHON: Knob = value(
	"siphon",
	"DIG_SIPHON",
	"pin the essence draw at <percent> on the vein under the crosshair (rides ore)",
);
pub const SPIKELOG: Knob = flag(
	"spikelog",
	"DIG_SPIKELOG",
	"log per-phase breakdowns of over-budget frames",
);
pub const CHUNK_SLOTS: Knob = value(
	"chunkslots",
	"DIG_CHUNK_SLOTS",
	"override the chunk slot count",
);

/// Every knob, in the order the usage text lists them. Both launchers
/// iterate this for their help, and the web harness mirrors it.
pub const ALL: &[Knob] = &[
	SCREEN,
	PLAYERBOX,
	CONFETTI,
	SHAPES,
	DOORS,
	WARD,
	VAULT,
	VAULT_DARK,
	ROOF,
	TUNNEL,
	DIVE,
	RIVER,
	TORCHES,
	TORCHES_FAR,
	TORCHES_WALL,
	ORE,
	NODE,
	MINING,
	CRACK,
	SIPHON,
	CAST,
	GI_ITERS,
	STUB,
	GI,
	SPIKELOG,
	CHUNK_SLOTS,
];

/// The known spellings of a `screen` value, for the error a typo raises.
const SCREEN_NAMES: &str = "menu|inventory|inventory-hover|alchemy|alchemy-hover|transmute|transmuting|\
	skills|spellbook|hand|bucket|bucket-full|torch|siphon|pick|sigil";

/// Read every knob through `get` and write what it means into `config`.
///
/// `get` answers with the raw value the launcher found, or `None` when
/// the knob was not set — a flag's value is ignored beyond its presence.
/// Knobs that depend on one another (`siphon` rides `ore`, `vaultdark`
/// implies `vault`) are resolved here, so neither launcher can get the
/// order wrong. An explicit pitch already in `config` survives the
/// siphon's default.
pub fn apply(
	config: &mut EngineConfig,
	get: &dyn Fn(&Knob) -> Option<String>,
) -> Result<(), String> {
	let on = |k: &Knob| get(k).is_some();
	let percent = |k: &Knob| -> Result<Option<f32>, String> {
		match get(k) {
			None => Ok(None),
			Some(v) => match v.parse::<f32>() {
				Ok(p) if (0.0..=100.0).contains(&p) => Ok(Some(p / 100.0)),
				_ => Err(format!(
					"{} must be a percent in 0..=100, got {v:?}",
					k.name
				)),
			},
		}
	};
	let count = |k: &Knob| -> Result<Option<i32>, String> {
		match get(k) {
			None => Ok(None),
			Some(v) => v
				.parse()
				.map(Some)
				.map_err(|_| format!("{} must be a count, got {v:?}", k.name)),
		}
	};

	if let Some(v) = get(&PLAYERBOX) {
		config.debug_playerbox = Some(parse_held_item(&v).ok_or_else(|| {
			format!(
				"{}: unknown item {v:?} — known: dirt, torch, stairs, slab, door, \
				 bucket, bucket-full, siphon, pick, sigil, empty",
				PLAYERBOX.name
			)
		})?);
	}
	config.debug_confetti = on(&CONFETTI);
	config.debug_shapes = on(&SHAPES);
	config.debug_doors = on(&DOORS);
	config.debug_vault_dark = on(&VAULT_DARK);
	config.debug_vault = config.debug_vault_dark || on(&VAULT);
	config.debug_roof = on(&ROOF);
	config.debug_tunnel = on(&TUNNEL);
	config.debug_dive = on(&DIVE);
	config.debug_river = on(&RIVER);
	config.debug_torches_wall = on(&TORCHES_WALL);
	if let Some(v) = get(&ORE) {
		config.debug_ore = v
			.parse()
			.ok()
			.filter(|t| (1..=4).contains(t))
			.ok_or_else(|| format!("{} wants a tier 1..=4, got {v:?}", ORE.name))?;
	}
	if let Some(v) = get(&NODE) {
		config.debug_node = Some(
			parse_node_kind(&v).ok_or_else(|| format!("{}: unknown node kind {v:?}", NODE.name))?,
		);
	}
	if let Some(v) = get(&MINING) {
		config.debug_mining = v
			.parse()
			.map_err(|_| format!("{} wants a level, got {v:?}", MINING.name))?;
	}
	config.debug_crack = percent(&CRACK)?;
	if let Some(r) = count(&WARD)? {
		if r < 1 {
			return Err(format!(
				"{} wants a radius of at least one, got {r}",
				WARD.name
			));
		}
		config.debug_ward = Some(r);
	}
	if let Some(n) = count(&TORCHES)? {
		config.debug_torches = n;
	}
	if let Some(n) = count(&TORCHES_FAR)? {
		config.debug_torches_far = n;
	}
	if let Some(v) = get(&GI_ITERS) {
		let (gather, sun) = v
			.split_once(',')
			.and_then(|(g, s)| Some((g.trim().parse().ok()?, s.trim().parse().ok()?)))
			.ok_or_else(|| {
				format!(
					"{} wants <gather>,<sun> iteration counts, got {v:?}",
					GI_ITERS.name
				)
			})?;
		config.gi_gather_iters = gather;
		config.gi_sun_iters = sun;
	}
	if let Some(v) = get(&STUB) {
		let (mask, bad) = crate::renderer::stub::parse(&v);
		if !bad.is_empty() {
			return Err(format!(
				"unknown {} term(s): {} — known: {}",
				STUB.name,
				bad.join(", "),
				crate::renderer::stub::names()
					.collect::<Vec<_>>()
					.join(", ")
			));
		}
		config.stub = mask;
	}
	match get(&GI).as_deref() {
		None => {}
		Some("window") => config.debug_gi = true,
		Some("probes") => {
			config.debug_gi = true;
			config.debug_gi_probes = true;
		}
		Some(v) => return Err(format!("{} wants window or probes, got {v:?}", GI.name)),
	}
	if let Some(v) = get(&SCREEN) {
		config.debug_screen = Some(DebugScreen::parse(&v).ok_or_else(|| {
			format!(
				"{}: unknown screen {v:?} — known: {SCREEN_NAMES}",
				SCREEN.name
			)
		})?);
	}
	config.debug_cast = percent(&CAST)?;
	config.debug_siphon = percent(&SIPHON)?;
	if config.debug_siphon.is_some() {
		// A vein is derived worldgen and cannot be placed, so the draw
		// rides the ore chamber; without one the crosshair holds bare
		// rock and the run photographs a wall. The tool has to be in
		// hand, and the ray has to be LEVEL — the fly camera starts
		// pitched down at the chamber's floor plate.
		if config.debug_ore == 0 {
			return Err(format!(
				"{} needs a vein to draw from: set {}=<tier 1..=4> too",
				SIPHON.name, ORE.name
			));
		}
		config
			.debug_screen
			.get_or_insert(DebugScreen::Tool(BlockType::EssenceSiphon));
		config.pitch.get_or_insert(0.0);
	}
	config.spike_log = on(&SPIKELOG);
	if let Some(v) = get(&CHUNK_SLOTS) {
		config.chunk_slots_override = Some(
			v.parse::<u32>()
				.ok()
				.filter(|&n| n > 0)
				.ok_or_else(|| format!("bad value for {}: {v}", CHUNK_SLOTS.name))?,
		);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::cell::RefCell;
	use std::collections::BTreeSet;

	#[test]
	fn every_knob_is_read_and_spelled_once() {
		let asked = RefCell::new(BTreeSet::new());
		let mut config = EngineConfig::desktop();
		apply(&mut config, &|k| {
			asked.borrow_mut().insert(k.name);
			None
		})
		.unwrap();
		let asked = asked.into_inner();
		for k in ALL {
			assert!(
				asked.contains(k.name),
				"{} is in the table but never read",
				k.name
			);
			assert!(
				k.name
					.chars()
					.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
				"{} must be a bare lowercase word: it is a URL key",
				k.name
			);
			assert!(k.env.starts_with("DIG_"), "{} must read from DIG_*", k.env);
		}
		assert_eq!(
			asked.len(),
			ALL.len(),
			"an arm reads a knob the table does not list"
		);
		let names: BTreeSet<_> = ALL.iter().map(|k| k.name).collect();
		let envs: BTreeSet<_> = ALL.iter().map(|k| k.env).collect();
		assert_eq!(names.len(), ALL.len());
		assert_eq!(envs.len(), ALL.len());
	}

	#[test]
	fn a_flag_is_on_by_presence_and_a_bad_value_is_an_error() {
		let mut config = EngineConfig::desktop();
		apply(&mut config, &|k| (k.name == VAULT.name).then(|| "0".into())).unwrap();
		assert!(config.debug_vault);

		let mut config = EngineConfig::desktop();
		apply(&mut config, &|k| {
			(k.name == VAULT_DARK.name).then(String::new)
		})
		.unwrap();
		assert!(config.debug_vault && config.debug_vault_dark);

		for (knob, bad) in [
			(&ORE, "7"),
			(&CRACK, "101"),
			(&SCREEN, "alchemyy"),
			(&NODE, "emberore"),
			(&GI, "both"),
			(&STUB, "wsun,wnope"),
			(&GI_ITERS, "4"),
			(&CHUNK_SLOTS, "0"),
		] {
			let mut config = EngineConfig::desktop();
			let err = apply(&mut config, &|k| (k.name == knob.name).then(|| bad.into()))
				.expect_err(knob.name);
			assert!(err.contains(knob.name), "{err}");
		}
	}

	#[test]
	fn the_siphon_rides_the_ore_chamber_and_levels_the_ray() {
		let mut config = EngineConfig::desktop();
		let err = apply(&mut config, &|k| {
			(k.name == SIPHON.name).then(|| "50".into())
		})
		.unwrap_err();
		assert!(err.contains(ORE.name));

		let mut config = EngineConfig::desktop();
		config.pitch = Some(-20.0);
		apply(&mut config, &|k| match k.name {
			n if n == SIPHON.name => Some("50".into()),
			n if n == ORE.name => Some("2".into()),
			_ => None,
		})
		.unwrap();
		assert_eq!(
			config.debug_screen,
			Some(DebugScreen::Tool(BlockType::EssenceSiphon))
		);
		assert_eq!(config.pitch, Some(-20.0), "an explicit pitch wins");
		assert_eq!(config.debug_ore, 2);

		let mut config = EngineConfig::desktop();
		apply(&mut config, &|k| match k.name {
			n if n == SIPHON.name => Some("50".into()),
			n if n == ORE.name => Some("2".into()),
			_ => None,
		})
		.unwrap();
		assert_eq!(config.pitch, Some(0.0));
	}
}
