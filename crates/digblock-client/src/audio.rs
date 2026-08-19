//! The audio seam: the engine emits [`AudioCmd`]s into an injected
//! [`AudioSink`] and never touches a device — mirroring the
//! `MenuServices`/`NetTransport` pattern that keeps the engine
//! platform-free. Desktop fills the seam with rodio on its own thread
//! (`apps/desktop/src/audio.rs`); web applies commands synchronously to
//! a WebAudio graph (`apps/web/src/audio.rs`); iOS injects nothing and
//! stays silent. A `None` sink costs one branch per would-be emit, so
//! bench/screenshot runs are untouched.
//!
//! This module also owns the pure, headless-testable emission logic
//! that doesn't fit a single call site: footstep cadence and the
//! ambient-bed classifier.

use digblock_core::block::BlockType;

/// Sound identifiers, 1:1 with the OGGs in `digblock_assets::sfx`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sfx {
	DigStone,
	DigEarth,
	DigWood,
	BreakStone,
	BreakEarth,
	BreakWood,
	Place,
	StepStone,
	StepEarth,
	StepWood,
	Waterfall,
	Splash,
	Swim,
	AmbientWind,
	AmbientWater,
	AmbientCave,
	SiphonDraw,
	SiphonTake,
	LevelUp,
	LevelUnlock,
}

/// The embedded bytes for a sound — the enum↔asset pairing lives here
/// and nowhere else; both platform fillers key their decode caches off
/// this.
pub fn bytes(s: Sfx) -> &'static [u8] {
	use digblock_assets::sfx as a;
	match s {
		Sfx::DigStone => a::DIG_STONE,
		Sfx::DigEarth => a::DIG_EARTH,
		Sfx::DigWood => a::DIG_WOOD,
		Sfx::BreakStone => a::BREAK_STONE,
		Sfx::BreakEarth => a::BREAK_EARTH,
		Sfx::BreakWood => a::BREAK_WOOD,
		Sfx::Place => a::PLACE,
		Sfx::StepStone => a::STEP_STONE,
		Sfx::StepEarth => a::STEP_EARTH,
		Sfx::StepWood => a::STEP_WOOD,
		Sfx::Waterfall => a::WATERFALL,
		Sfx::Splash => a::SPLASH,
		Sfx::Swim => a::SWIM,
		Sfx::AmbientWind => a::AMBIENT_WIND,
		Sfx::AmbientWater => a::AMBIENT_WATER,
		Sfx::AmbientCave => a::AMBIENT_CAVE,
		Sfx::SiphonDraw => a::SIPHON_DRAW,
		Sfx::SiphonTake => a::SIPHON_TAKE,
		Sfx::LevelUp => a::LEVEL_UP,
		Sfx::LevelUnlock => a::LEVEL_UNLOCK,
	}
}

/// Every playable [`Sfx`], for filler decode caches and tests.
pub const ALL_SFX: [Sfx; 20] = [
	Sfx::DigStone,
	Sfx::DigEarth,
	Sfx::DigWood,
	Sfx::BreakStone,
	Sfx::BreakEarth,
	Sfx::BreakWood,
	Sfx::Place,
	Sfx::StepStone,
	Sfx::StepEarth,
	Sfx::StepWood,
	Sfx::Waterfall,
	Sfx::Splash,
	Sfx::Swim,
	Sfx::AmbientWind,
	Sfx::AmbientWater,
	Sfx::AmbientCave,
	Sfx::SiphonDraw,
	Sfx::SiphonTake,
	Sfx::LevelUp,
	Sfx::LevelUnlock,
];

/// The fanfare a level-up speaks: the plain chime, or the bigger one
/// when that level opened something (see
/// [`digblock_core::unlock::opens`]).
pub fn level_sfx(unlocked: bool) -> Sfx {
	if unlocked {
		Sfx::LevelUnlock
	} else {
		Sfx::LevelUp
	}
}

/// Non-positional gain for the level fanfare — it is the player's own
/// event, so it plays at the ears like a UI sound.
pub const LEVEL_GAIN: f32 = 0.7;

/// Sound-material class of a block (dig/break/step variants).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Material {
	Stone,
	Earth,
	Wood,
}

/// Which material a block sounds like — derived from what the block IS, never
/// from a roster.
///
/// §7.2's substance axis answers this for every real block, and it is already
/// what decides a block's price, yield and unlock. Functional items have no
/// substance by design; those fall to the `DigFamily` their row declares.
///
/// Both matches are EXHAUSTIVE on purpose. A hand-listed roster ending in
/// `_ => Earth` silently missed every row §7.2 added: mining an Aetherock seam,
/// breaking crystal or walking a metal floor all played the sound of digging
/// soil. With no catch-all arm, a new material or dig family fails the BUILD
/// instead.
pub fn material_of(b: BlockType) -> Material {
	use digblock_core::block::DigFamily;
	use digblock_core::material::Material as Subst;
	if let Some(m) = b.material() {
		return match m {
			// Timber, growing or planed — and the canopy, which is closer
			// to wood than to either of the others.
			Subst::Wood
			| Subst::Lumber
			| Subst::DarkLumber
			| Subst::RedLumber
			| Subst::Leaf
			| Subst::PineLeaf
			| Subst::Heartwood
			| Subst::DrainedHeartwood => Material::Wood,
			// Rock and everything won from it: dressed stone, glass, the
			// refined substances, and the ores in the wall.
			Subst::Stone
			| Subst::StoneBrick
			| Subst::PolishedStone
			| Subst::Sandstone
			| Subst::SandstoneBrick
			| Subst::Glass
			| Subst::Metal
			| Subst::Crystal
			| Subst::Rubble
			| Subst::Gravel
			| Subst::Emberore
			| Subst::Duskstone
			| Subst::Glowvein
			| Subst::Aetherock
			| Subst::DepletedStone
			| Subst::DepletedEmberore
			| Subst::DepletedDuskstone
			| Subst::DepletedGlowvein
			| Subst::DepletedAetherock
			| Subst::Leystone
			| Subst::SpentLeystone => Material::Stone,
			// Loose ground — the spring bed included, sand that it is.
			Subst::Dirt
			| Subst::Sand
			| Subst::Snow
			| Subst::Grass
			| Subst::DryGrass
			| Subst::Mud
			| Subst::Springstone
			| Subst::SpentSpringstone => Material::Earth,
		};
	}
	match b.dig_family() {
		DigFamily::Rock => Material::Stone,
		DigFamily::Wood => Material::Wood,
		DigFamily::Soft | DigFamily::Fixed => Material::Earth,
	}
}

pub fn dig_sfx(m: Material) -> Sfx {
	match m {
		Material::Stone => Sfx::DigStone,
		Material::Earth => Sfx::DigEarth,
		Material::Wood => Sfx::DigWood,
	}
}

pub fn break_sfx(m: Material) -> Sfx {
	match m {
		Material::Stone => Sfx::BreakStone,
		Material::Earth => Sfx::BreakEarth,
		Material::Wood => Sfx::BreakWood,
	}
}

/// The two sounds a hold speaks: the loop while it runs, and the one-shot it
/// lands on. `extracting` says whether this hold is a field draw rather than a
/// dig.
///
/// One function because the pair has to agree with itself — a hold that hums and
/// then shatters is worse than one that does neither — and because the pair is
/// exactly what a draw has to override. Resolving both halves through
/// [`material_of`], which for an ore is rock, made drawing a seam sound
/// precisely like quarrying one out.
///
/// The `block` each half is asked about is still the caller's, because the two
/// are read at different moments: the loop at the hold's start, the finish
/// against the cell as it stood when it gave way.
pub fn dig_voice(extracting: bool, block: BlockType) -> (Sfx, Sfx) {
	if extracting {
		(Sfx::SiphonDraw, Sfx::SiphonTake)
	} else {
		let m = material_of(block);
		(dig_sfx(m), break_sfx(m))
	}
}

pub fn step_sfx(m: Material) -> Sfx {
	match m {
		Material::Stone => Sfx::StepStone,
		Material::Earth => Sfx::StepEarth,
		Material::Wood => Sfx::StepWood,
	}
}

/// Stable handle for a running loop voice.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LoopId(pub u32);

/// The hold-to-dig loop (one at a time).
pub const LOOP_DIG: LoopId = LoopId(1);
/// The non-positional ambient bed (one at a time; restarted on change).
pub const LOOP_AMBIENT: LoopId = LoopId(2);
/// Water voices allocate upward from here (see `water_audio.rs`).
pub const LOOP_WATER_BASE: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioCmd {
	/// Where the ears are. Sent once per frame after the camera settles.
	Listener {
		pos: [f32; 3],
		forward: [f32; 3],
	},
	/// One-shot. `pos: None` = non-positional (UI-ish, full volume).
	Sfx {
		sound: Sfx,
		pos: Option<[f32; 3]>,
		gain: f32,
	},
	/// Start (or restart) a loop voice under a stable id.
	LoopStart {
		id: LoopId,
		sound: Sfx,
		pos: Option<[f32; 3]>,
		gain: f32,
	},
	/// Move/retune a running loop voice; ignored for unknown ids.
	LoopMove {
		id: LoopId,
		pos: [f32; 3],
		gain: f32,
	},
	/// Playback-rate multiplier for a running loop (dig pitch ramp).
	LoopPitch {
		id: LoopId,
		rate: f32,
	},
	LoopStop {
		id: LoopId,
	},
	/// App suspended / tab hidden: silence everything, keep state.
	Pause,
	Resume,
	/// User volume settings, multipliers on the authored gains (unity
	/// leaves them as authored, and they may go above it). Applies to
	/// running voices too — a stationary loop must not wait for its next
	/// `LoopMove`.
	Volume {
		music: f32,
		sfx: f32,
	},
	/// Which music pool to draw from. Sent when the world clock crosses
	/// a horizon, and once at startup. Deliberately advisory: fillers
	/// switch at the next track boundary rather than cutting the one
	/// that's playing, so dusk doesn't chop a track in half.
	MusicSet {
		night: bool,
	},
}

/// A shuffled permutation of `0..n`, from `seed`. Both fillers use this
/// so neither needs an RNG dependency of its own, and the shuffle is
/// testable without an audio device.
///
/// Fisher-Yates over a SplitMix64 stream: the playlist is a handful of
/// tracks, so all this has to be is unbiased and cheap.
pub fn shuffle_order(seed: u64, n: usize) -> Vec<usize> {
	let mut order: Vec<usize> = (0..n).collect();
	let mut state = seed;
	let mut next = move || {
		state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
		let mut z = state;
		z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
		z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
		z ^ (z >> 31)
	};
	for i in (1..n).rev() {
		let j = (next() % (i as u64 + 1)) as usize;
		order.swap(i, j);
	}
	order
}

/// The injected backend. Deliberately not `Send` — the web filler holds
/// JS values (precedent: `NetTransport`).
pub trait AudioSink {
	fn send(&mut self, cmd: AudioCmd);
}

// ---- footstep cadence -------------------------------------------------

/// Distance-driven footstep emitter: pure accumulator logic, engine
/// feeds it per-frame movement facts and forwards the returned step.
#[derive(Default)]
pub struct Footsteps {
	accum: f32,
	was_airborne: bool,
}

/// Horizontal distance between steps, blocks — so a walk's cadence falls
/// out of its speed. Cadence, not playback rate: the samples are
/// untouched, so stepping faster costs no pitch. Because this is a
/// distance accumulator, a slower walk still paces itself correctly.
const STRIDE: f32 = 1.1;
/// Gain of an ordinary step, and of the heavier one on landing.
const STEP_GAIN: f32 = 0.6;
const LAND_GAIN: f32 = 0.8;

impl Footsteps {
	/// Advance by one frame. `moved` = horizontal distance this frame;
	/// `grounded` = standing on `ground` (the block under the feet);
	/// `in_water` suppresses steps entirely. Returns a step's material
	/// and gain when one lands this frame.
	pub fn advance(
		&mut self,
		moved: f32,
		grounded: bool,
		ground: Option<BlockType>,
		in_water: bool,
	) -> Option<(Material, f32)> {
		if in_water || !grounded {
			self.accum = 0.0;
			self.was_airborne = !grounded && !in_water;
			return None;
		}
		let ground = ground?;
		// Landing: one immediate, slightly heavier step.
		if std::mem::take(&mut self.was_airborne) {
			self.accum = 0.0;
			return Some((material_of(ground), LAND_GAIN));
		}
		self.accum += moved;
		if self.accum >= STRIDE {
			self.accum -= STRIDE;
			Some((material_of(ground), STEP_GAIN))
		} else {
			None
		}
	}
}

// ---- swim strokes -----------------------------------------------------

/// Distance-driven swim-stroke emitter, the water counterpart of
/// [`Footsteps`] — a swimmer moving through water should sound like it,
/// and the underwater ambient bed alone is the same whether you drift or
/// sprint.
pub struct SwimStrokes {
	accum: f32,
}

impl Default for SwimStrokes {
	fn default() -> Self {
		Self {
			accum: STROKE * STROKE_HEAD_START,
		}
	}
}

/// Distance between strokes, blocks. Longer than [`STRIDE`]: a stroke
/// covers more ground than a footfall, and swim speed is slower, so
/// strokes come well apart in steady swimming.
const STROKE: f32 = 1.5;
/// Quieter than a footstep — the ear is already inside the water bed.
const STROKE_GAIN: f32 = 0.45;
/// Fraction of a stroke banked on entering the water, so the first one
/// lands promptly instead of a full stroke's silence after the splash.
const STROKE_HEAD_START: f32 = 0.6;

impl SwimStrokes {
	/// Advance by one frame. `moved` = horizontal distance this frame;
	/// `swimming` = in water and off the ground (wading keeps its
	/// footsteps, which are grounded-gated). Returns a gain when a
	/// stroke lands this frame.
	pub fn advance(&mut self, moved: f32, swimming: bool) -> Option<f32> {
		if !swimming {
			// Head start, not zero: leaving and re-entering shouldn't
			// have to re-earn the first stroke.
			self.accum = STROKE * STROKE_HEAD_START;
			return None;
		}
		self.accum += moved;
		if self.accum >= STROKE {
			self.accum -= STROKE;
			Some(STROKE_GAIN)
		} else {
			None
		}
	}

	/// World swap: forget the banked distance.
	pub fn reset(&mut self) {
		*self = Self::default();
	}
}

// ---- ambient bed ------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AmbientZone {
	Surface,
	Underwater,
	Cave,
}

pub fn ambient_sfx(z: AmbientZone) -> Sfx {
	match z {
		AmbientZone::Surface => Sfx::AmbientWind,
		AmbientZone::Underwater => Sfx::AmbientWater,
		AmbientZone::Cave => Sfx::AmbientCave,
	}
}

/// The prospector's sense (SKILLS-DESIGN §5.5): a faint shimmer near a
/// deposit you cannot yet see, paced so it is a hint rather than a
/// metronome.
///
/// A cadence emitter like `Footsteps`, but paced by TIME rather than by
/// distance — the point is "something is near", and a player standing
/// still at a wall should still hear it. Silence resets the clock, so
/// walking into range speaks at once instead of on the next tick of a
/// free-running timer.
pub struct Prospect {
	since: f32,
}

/// Starts READY, exactly as [`Prospect::reset`] leaves it. The derived
/// `Default` started at zero instead, so the very first deposit a session
/// walked up to sat silent for a full period — the one shimmer that most
/// needed to arrive on time, since it is the only one that tells a player
/// this sense exists at all.
impl Default for Prospect {
	fn default() -> Self {
		Self {
			since: PROSPECT_PERIOD_SECS,
		}
	}
}

/// Seconds between shimmers while in range.
pub const PROSPECT_PERIOD_SECS: f32 = 6.0;
/// Quiet enough to sit under the ambient bed — this is a suggestion, not
/// an alarm.
pub const PROSPECT_GAIN: f32 = 0.22;

impl Prospect {
	/// Advance by `dt` with the sense either in range or not; `Some(())`
	/// on the frames a shimmer should sound.
	pub fn advance(&mut self, in_range: bool, dt: f32) -> bool {
		if !in_range {
			// Out of range resets rather than pauses: crossing back in is
			// the interesting moment and it should not have to wait out
			// the remainder of a timer from somewhere else.
			self.since = PROSPECT_PERIOD_SECS;
			return false;
		}
		self.since += dt;
		if self.since >= PROSPECT_PERIOD_SECS {
			self.since = 0.0;
			return true;
		}
		false
	}

	pub fn reset(&mut self) {
		*self = Self::default();
	}
}

/// Ambient-bed state machine: classification comes from the engine
/// (submersion test + worldgen surface height); this owns the hysteresis
/// so boundary flicker never restarts the bed.
pub struct Ambient {
	current: AmbientZone,
	pending: AmbientZone,
	pending_secs: f32,
	started: bool,
}

/// A new classification must hold this long before the bed switches.
const AMBIENT_HYSTERESIS_SECS: f32 = 1.0;
pub const AMBIENT_GAIN: f32 = 0.25;

impl Default for Ambient {
	fn default() -> Self {
		Self {
			current: AmbientZone::Surface,
			pending: AmbientZone::Surface,
			pending_secs: 0.0,
			started: false,
		}
	}
}

impl Ambient {
	/// Feed this frame's classification; returns a zone when the bed
	/// should (re)start — the first call always starts the current bed.
	pub fn advance(&mut self, zone: AmbientZone, dt: f32) -> Option<AmbientZone> {
		if !self.started {
			self.started = true;
			self.current = zone;
			self.pending = zone;
			return Some(zone);
		}
		if zone != self.pending {
			self.pending = zone;
			self.pending_secs = 0.0;
			return None;
		}
		if zone == self.current {
			self.pending_secs = 0.0;
			return None;
		}
		self.pending_secs += dt;
		if self.pending_secs >= AMBIENT_HYSTERESIS_SECS {
			self.current = zone;
			self.pending_secs = 0.0;
			Some(zone)
		} else {
			None
		}
	}

	/// Reset so the next `advance` restarts the bed (world swap).
	pub fn reset(&mut self) {
		self.started = false;
		self.pending_secs = 0.0;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The rows §7.2 added, which the old hand-listed roster missed
	/// entirely — every one of them dug, broke and was walked on as DIRT.
	/// Spot-checked across all three classes, because "it compiles" only
	/// proves the match is exhaustive, not that the arms are sane.
	#[test]
	fn the_catalog_sounds_like_what_it_is_made_of() {
		use BlockType as B;
		use digblock_core::block::ShapeFamily;
		for b in [
			B::StoneBrick,
			B::PolishedStone,
			B::SandstoneBrick,
			B::Metal,
			B::Crystal,
			B::Glass,
			B::DepletedStone,
			B::Emberore,
			B::Duskstone,
			B::Glowvein,
			B::Aetherock,
			B::StoneDoor,
			B::Rubble,
		] {
			assert_eq!(material_of(b), Material::Stone, "{b:?}");
		}
		for b in [B::DarkLumber, B::RedLumber, B::Lumber, B::LumberDoor] {
			assert_eq!(material_of(b), Material::Wood, "{b:?}");
		}
		for b in [B::Dirt, B::Sand, B::Snow, B::DryGrass] {
			assert_eq!(material_of(b), Material::Earth, "{b:?}");
		}
		// Every shape of a substance sounds like the substance: a slab and
		// a stair are the same stuff as the cube they came from.
		//
		// The cube is `from_grid(m, Block)` — §7.2's own inverse, and the
		// lookup `slab_base` already uses. It used to be a `find` for the
		// material's row whose `shape_family()` was `None`, which no grid
		// row can ever satisfy: `None` is reserved for the FUNCTIONAL
		// shapes (torch, table, chair, bed) and every one of those is
		// material-LESS by construction, as `material_roster_is_pinned`
		// spells out. The predicate was unsatisfiable, so every iteration
		// took the `continue` and the assert never ran once.
		let mut worked = 0;
		for b in BlockType::ALL {
			let Some(m) = b.material() else { continue };
			let base = BlockType::from_grid(m, ShapeFamily::Block)
				.unwrap_or_else(|| panic!("{m:?} has no cube form to sound like"));
			assert_eq!(
				material_of(b),
				material_of(base),
				"{b:?} does not sound like the {m:?} it is made of"
			);
			worked += usize::from(b != base);
		}
		// And the guard the dead version needed: comparing every cube with
		// itself would also pass, so pin that real slabs, stairs and doors
		// were weighed against their cubes. A floor rather than a count —
		// the catalog only grows, and this should not need touching when
		// it does.
		assert!(
			worked > 20,
			"only {worked} worked shapes reached their cube"
		);
	}

	/// A draw and a dig speak different languages at BOTH ends, and the
	/// two vocabularies never overlap.
	///
	/// The disjointness is the real assertion. An extraction reads as a
	/// draw only for as long as nothing it plays is also what breaking
	/// rock plays — and rock is exactly what the old mapping resolved a
	/// vein to, so the failure mode here is not hypothetical.
	#[test]
	fn a_field_draw_never_sounds_like_a_dig() {
		use digblock_core::block::BlockType as B;
		for ore in [B::Emberore, B::Duskstone, B::Glowvein, B::Aetherock] {
			assert_eq!(
				dig_voice(true, ore),
				(Sfx::SiphonDraw, Sfx::SiphonTake),
				"{ore:?} drawn"
			);
			// The same block DUG — the spent rock left behind, quarried
			// with an ordinary tool — still sounds like the rock it is.
			assert_eq!(
				dig_voice(false, ore),
				(Sfx::DigStone, Sfx::BreakStone),
				"{ore:?} dug"
			);
		}
		let dig: Vec<Sfx> = [Material::Stone, Material::Earth, Material::Wood]
			.into_iter()
			.flat_map(|m| [dig_sfx(m), break_sfx(m)])
			.collect();
		for s in [Sfx::SiphonDraw, Sfx::SiphonTake] {
			assert!(!dig.contains(&s), "{s:?} doubles as a dig sound");
		}
	}

	#[test]
	fn footsteps_pace_by_distance_and_stop_when_airborne() {
		let mut f = Footsteps::default();
		let ground = Some(BlockType::Grass);
		let mut steps = 0;
		// A second of walking at walk speed: a few blocks of ground, and
		// the strides that pace out of it.
		for _ in 0..60 {
			if f.advance(4.0 / 60.0, true, ground, false).is_some() {
				steps += 1;
			}
		}
		assert!((3..=4).contains(&steps), "{steps}");
		// Airborne: accumulator resets, no steps.
		assert!(f.advance(1.0, false, None, false).is_none());
		assert!(f.advance(1.0, false, None, false).is_none());
		// Landing emits one heavier step immediately.
		let landed = f.advance(0.0, true, ground, false);
		assert_eq!(landed, Some((Material::Earth, LAND_GAIN)));
		// Swimming: silent even while "grounded".
		assert!(f.advance(1.0, true, ground, true).is_none());
	}

	#[test]
	fn swim_strokes_pace_by_distance_and_stop_when_still() {
		let mut s = SwimStrokes::default();
		// Entering the water banks a head start, so the first stroke
		// lands well inside the first stroke's worth of travel.
		assert!(s.advance(0.0, false).is_none(), "not swimming yet");
		assert_eq!(s.advance(STROKE * 0.5, true), Some(STROKE_GAIN));
		// Several seconds of steady swimming, and the strokes that pace
		// out of that much travel.
		let mut strokes = 0;
		for _ in 0..240 {
			if s.advance(3.0 / 60.0, true).is_some() {
				strokes += 1;
			}
		}
		assert!((7..=9).contains(&strokes), "{strokes}");
		// Floating still is silent, however long you float.
		for _ in 0..600 {
			assert!(s.advance(0.0, true).is_none());
		}
		// And so is standing on the bottom — wading is the footsteps'
		// job, and both firing would double up.
		assert!(s.advance(1.0, false).is_none());
	}

	#[test]
	fn shuffle_is_a_permutation_and_varies_with_the_seed() {
		for n in [0usize, 1, 2, 3, 8] {
			let mut sorted = shuffle_order(0xDEAD_BEEF, n);
			assert_eq!(sorted.len(), n);
			sorted.sort_unstable();
			assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "n={n}");
		}
		// The whole point: two launches don't open with the same track.
		let orders: std::collections::HashSet<Vec<usize>> =
			(0..16).map(|s| shuffle_order(s, 6)).collect();
		assert!(orders.len() > 8, "seeds barely move the order");
	}

	#[test]
	fn ambient_switches_only_after_hysteresis() {
		let mut a = Ambient::default();
		// First frame starts the surface bed.
		assert_eq!(
			a.advance(AmbientZone::Surface, 0.016),
			Some(AmbientZone::Surface)
		);
		// A brief dip underwater (shorter than the hysteresis) is ignored.
		for _ in 0..30 {
			assert_eq!(a.advance(AmbientZone::Underwater, 0.016), None);
		}
		assert_eq!(a.advance(AmbientZone::Surface, 0.016), None);
		// A sustained dive switches once.
		let mut switched = 0;
		for _ in 0..90 {
			if a.advance(AmbientZone::Underwater, 0.016).is_some() {
				switched += 1;
			}
		}
		assert_eq!(switched, 1);
	}
}
