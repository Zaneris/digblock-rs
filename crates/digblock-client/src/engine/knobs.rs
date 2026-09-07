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

/// Whether a knob is a switch or carries a value — what the desktop's
/// `--knobs` listing spells with a value placeholder, so the harness
/// that shows that listing offers the shape the page will accept.
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
	/// One line for the desktop's `--knobs` listing, which is also the
	/// web harness's usage text for the scenes.
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
pub const CLOUDS: Knob = flag(
	"clouds",
	"DIG_CLOUDS",
	"the volumetric cloud deck, off by default while it is tuned",
);
pub const ORBIT: Knob = value(
	"orbit",
	"DIG_ORBIT",
	"orbit the aim point: <radius>[,<deg/s>] blocks (cinematic camera scene)",
);
pub const DOLLY: Knob = value(
	"dolly",
	"DIG_DOLLY",
	"tracking dolly past the aim point: <secs>[,<blocks>] (cinematic camera scene)",
);
pub const TUTORIAL: Knob = value(
	"tutorial",
	"DIG_TUTORIAL",
	"force the walkthrough: on|off|<step> \
	 (move|look|jump|dig|switch|slot|place|menu|inventory|close|bench|tools|pick|craft|siphon|\
	 arcana|wispbar|closebook|wisp)",
);
pub const BURSTS: Knob = flag(
	"bursts",
	"DIG_BURSTS",
	"the burst rack: all four particle kinds thrown a few blocks ahead",
);
pub const NO_MARKERS: Knob = flag(
	"nomarkers",
	"DIG_NO_MARKERS",
	"drop the standing arrival markers (the spawn spiral) from the frame",
);
pub const SHAPES: Knob = flag(
	"shapes",
	"DIG_SHAPES",
	"every shaped block laid out in a row",
);
pub const MATERIALS: Knob = flag(
	"materials",
	"DIG_MATERIALS",
	"tiled stone, polished stone and metal with lamps for material review",
);
pub const DOORS: Knob = flag("doors", "DIG_DOORS", "doors in every state");
pub const EDITOR: Knob = flag(
	"editor",
	"DIG_EDITOR",
	"the shape editor: sculpt sub-block objects and save them as .shape files",
);
pub const SPHERE: Knob = value(
	"sphere",
	"DIG_SPHERE",
	"a glass sphere of <radius> closing about spawn, with no cast behind it",
);
pub const SPHERE_AGE: Knob = value(
	"sphereage",
	"DIG_SPHERE_AGE",
	"hold the sphere's climb at <secs> in (rides DIG_SPHERE)",
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
pub const GPU_TIMER: Knob = value(
	"gputimer",
	"DIG_GPU_TIMER",
	"GPU timestamp queries, 1 or 0 (desktop defaults on, the web off: see EngineConfig::web)",
);
pub const FPS_CAP: Knob = value(
	"fpscap",
	"DIG_FPS_CAP",
	"present at most <hz> frames a second (0 = the display's pace)",
);
pub const CHUNK_SLOTS: Knob = value(
	"chunkslots",
	"DIG_CHUNK_SLOTS",
	"override the chunk slot count",
);
pub const GI_RATE: Knob = value(
	"girate",
	"DIG_GI_RATE",
	"the GI resolve's adaptive-budget refresh distances as <indirect>,<sun> blocks (0 = every frame)",
);

/// Every knob, in the order the usage text lists them. The desktop
/// prints this table for `--knobs` and refuses a `DIG_*` variable
/// outside it; the web page refuses a `?key` outside it.
pub const ALL: &[Knob] = &[
	SCREEN,
	PLAYERBOX,
	CONFETTI,
	CLOUDS,
	ORBIT,
	DOLLY,
	TUTORIAL,
	BURSTS,
	NO_MARKERS,
	SHAPES,
	MATERIALS,
	DOORS,
	EDITOR,
	SPHERE,
	SPHERE_AGE,
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
	GI_RATE,
	STUB,
	GI,
	SPIKELOG,
	CHUNK_SLOTS,
	GPU_TIMER,
	FPS_CAP,
];

/// The known spellings of a `screen` value, for the error a typo raises.
const SCREEN_NAMES: &str = "menu|inventory|inventory-hover|alchemy|alchemy-hover|transmute|transmuting|\
	skills|hiscores|spellbook|spellbook-hover|hand|bucket|bucket-full|torch|siphon|pick|sigil";

/// Read every knob through `get` and write what it means into `config`. `get` answers with the raw
/// value the launcher found, or `None` when the knob was not set — a flag's value is ignored beyond
/// its presence. Knobs that depend on one another (`siphon` rides `ore`, `vaultdark` implies
/// `vault`) are resolved here, so neither launcher can get the order wrong. An explicit pitch
/// already in `config` survives the siphon's default.
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
	config.clouds = on(&CLOUDS);
	config.debug_bursts = on(&BURSTS);
	if let Some(v) = get(&TUTORIAL) {
		use crate::tutorial::{StepId, TutorialOverride};
		config.debug_tutorial = Some(match v.as_str() {
			"on" | "1" => TutorialOverride::On,
			"off" | "0" => TutorialOverride::Off,
			s => match StepId::parse(s) {
				Some(step) => TutorialOverride::At(step),
				None => {
					return Err(format!(
						"{} must be on, off, or a step name, got {v:?}",
						TUTORIAL.name
					));
				}
			},
		});
	}
	if let Some(v) = get(&ORBIT) {
		let mut parts = v.splitn(2, ',');
		let radius = parts.next().unwrap_or("").trim().parse::<f32>();
		let degs = match parts.next() {
			None => Ok(12.0),
			Some(d) => d.trim().parse::<f32>(),
		};
		match (radius, degs) {
			(Ok(r), Ok(d)) if r > 0.0 => config.debug_orbit = Some((r, d)),
			_ => {
				return Err(format!(
					"{} must be <radius>[,<deg/s>], got {v:?}",
					ORBIT.name
				));
			}
		}
	}
	if let Some(v) = get(&DOLLY) {
		let mut parts = v.splitn(2, ',');
		let secs = parts.next().unwrap_or("").trim().parse::<f32>();
		let blocks = match parts.next() {
			None => Ok(10.0),
			Some(d) => d.trim().parse::<f32>(),
		};
		match (secs, blocks) {
			(Ok(s), Ok(b)) if s > 0.0 && b > 0.0 => config.debug_dolly = Some((s, b)),
			_ => {
				return Err(format!(
					"{} must be <secs>[,<blocks>], got {v:?}",
					DOLLY.name
				));
			}
		}
	}
	config.markers = !on(&NO_MARKERS);
	config.debug_shapes = on(&SHAPES);
	config.debug_materials = on(&MATERIALS);
	config.debug_doors = on(&DOORS);
	config.debug_editor = on(&EDITOR);
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
	if let Some(r) = count(&SPHERE)? {
		if r < 1 {
			return Err(format!(
				"{} wants a radius of at least one, got {r}",
				SPHERE.name
			));
		}
		config.debug_sphere = Some(r);
	}
	if let Some(v) = get(&SPHERE_AGE) {
		if config.debug_sphere.is_none() {
			return Err(format!("{} rides {}", SPHERE_AGE.name, SPHERE.name));
		}
		config.debug_sphere_age = Some(
			v.parse()
				.map_err(|_| format!("{} wants seconds, got {v:?}", SPHERE_AGE.name))?,
		);
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
	if let Some(v) = get(&GI_RATE) {
		let (ind, sun) = v
			.split_once(',')
			.and_then(|(i, s)| Some((i.trim().parse::<f32>().ok()?, s.trim().parse::<f32>().ok()?)))
			.filter(|(i, s)| i.is_finite() && s.is_finite() && *i >= 0.0 && *s >= 0.0)
			.ok_or_else(|| {
				format!(
					"{} wants <indirect>,<sun> refresh distances in blocks (0 = every frame), got {v:?}",
					GI_RATE.name
				)
			})?;
		config.gi_rate_blocks = [ind, sun];
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
	if let Some(v) = get(&GPU_TIMER) {
		config.gpu_timer = match v.trim() {
			"1" | "on" => true,
			"0" | "off" => false,
			_ => return Err(format!("{} wants 1 or 0, got {v:?}", GPU_TIMER.name)),
		};
	}
	if let Some(v) = get(&FPS_CAP) {
		let hz = v
			.parse::<f32>()
			.ok()
			.filter(|hz| hz.is_finite() && *hz >= 0.0)
			.ok_or_else(|| format!("{} wants a frame rate in Hz, got {v:?}", FPS_CAP.name))?;
		config.fps_cap = (hz > 0.0).then_some(hz);
	}
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
			(&GI_RATE, "16"),
			(&GI_RATE, "-1,8"),
			(&CHUNK_SLOTS, "0"),
			(&FPS_CAP, "-60"),
			(&FPS_CAP, "sixty"),
			(&GPU_TIMER, "yes"),
		] {
			let mut config = EngineConfig::desktop();
			let err = apply(&mut config, &|k| (k.name == knob.name).then(|| bad.into()))
				.expect_err(knob.name);
			assert!(err.contains(knob.name), "{err}");
		}
	}

	/// The list a typo's error prints is prose sitting beside a match arm, and the two drift the
	/// moment a screen is added — so every spelling it offers has to BE one, and no two may name
	/// the same screen. The other direction, a variant nobody can spell, is held where it can fail
	/// to COMPILE: `screens`'s `fixture_of` matches exhaustively over [`DebugScreen`].
	#[test]
	fn every_screen_the_error_offers_can_be_asked_for() {
		let mut seen = std::collections::HashSet::new();
		for name in SCREEN_NAMES.split('|') {
			let name = name.trim();
			let screen = DebugScreen::parse(name)
				.unwrap_or_else(|| panic!("{name:?} is offered but does not parse"));
			assert!(
				seen.insert(format!("{screen:?}")),
				"{name:?} names a screen the list already offers"
			);
		}
		assert_eq!(
			seen.len(),
			SCREEN_NAMES.split('|').count(),
			"every offered spelling reached a screen of its own"
		);
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
