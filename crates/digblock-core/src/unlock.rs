//! Every level threshold in the game, and the one question each gate asks.
//! Nothing else may hold an unlock number, and no caller compares levels itself.
//!
//! - **The levels** — [`shape`] and [`material`] (§7.2's two grid axes),
//!   [`functional`] for the hand-priced items, plus [`mine`] and the two reveal
//!   axes. Each is the sole authority for its own axis, and the two questions
//!   players actually ask are DERIVED: [`craft`] is `max(shape, material)` and
//!   [`pickup`] reads `craft` of the drop.
//! - **The questions** — [`can_craft`], [`dig_blocked`], asked identically by
//!   client prediction, the server's `DigStart` backstop, the bot's target
//!   filter and the test harness, so all four agree by construction.
//! - **What a level opens** — [`opens`], DERIVED from those tables, so retuning
//!   a threshold moves the fanfare and the character screen's teasers with it.
//!
//! No wording lives here. A gate answers with a [`Requirement`] — the
//! block, the skill and the level — and the client's `voice` is what turns
//! one into a sentence, which is why the wire sends a REASON and never a
//! sentence and why a translation never touches this file.

use crate::block::{BlockType, ShapeFamily};
use crate::material::Material;
use crate::skills::{MAX_LEVEL, Skill, SkillSet};

// ---- the tables ------------------------------------------------------

/// The Alchemy level that opens a SHAPE — can you work any substance into
/// this form at all? (SKILLS-DESIGN §7.2's shape axis.)
///
/// The full cube sits at the very first level deliberately: the
/// progression belongs to the
/// materials, so a substance arrives in every form you have already
/// learned rather than dribbling in one shape at a time. Reaching a shape
/// LATER then hands it to you in every material you already own.
pub fn shape(s: ShapeFamily) -> u32 {
	match s {
		ShapeFamily::Block | ShapeFamily::Slab => 1,
		ShapeFamily::Stairs => 5,
		// The door lands WITH the first material that authors one, so the
		// shape fanfare and the first actual door are the same level-up —
		// it used to open early, and an unlock that hands you nothing
		// teaches players to ignore unlocks.
		ShapeFamily::Door => 12,
	}
}

/// The Alchemy level that opens a MATERIAL — when does this substance
/// become workable at all? `None` = never, in any shape.
///
/// A level here is not a promise that any particular cell is craftable:
/// [`craft`] refuses anything the world hands you, so Dirt has a level and
/// no craftable form, because the world gives dirt away and nobody has
/// authored a dirt slab. Both halves are deliberate — the level is what a
/// dirt slab WOULD open at, and the grid exists so that authoring one is a
/// block row and a PNG.
///
/// Materials with no authored block yet are listed for the same reason:
/// the level IS the design.
pub fn material(m: Material) -> Option<u32> {
	use Material as M;
	Some(match m {
		// The starter timber and the loose ground under your feet — what
		// the world hands a fresh player, so the bench has something to do
		// on day one.
		M::Wood => 1,
		M::Dirt | M::Sand => 5,
		// Planed timber: the joinery material.
		M::Lumber => 12,
		// The mason's unlock (§7.1) — clean rock exists only pristine in
		// terrain or transmuted here, which is what makes Rubble matter.
		M::Stone => 15,
		M::DarkLumber | M::StoneBrick => 18,
		M::Sandstone | M::Snow | M::Grass | M::Mud | M::Glass => 21,
		// The stained panes ride one rung past plain glass — the same
		// substance, asked for a colour.
		M::RedGlass | M::GreenGlass | M::BlueGlass | M::AmberGlass => 24,
		M::RedLumber => 27,
		M::PolishedStone | M::SandstoneBrick => 30,
		// Scarce by construction: Ferrum comes only from ore.
		M::Metal => 33,
		M::Crystal => 40,
		M::Rubble
		| M::Gravel
		| M::DryGrass
		| M::Leaf
		| M::PineLeaf
		| M::Emberore
		| M::Duskstone
		| M::Glowvein
		| M::Aetherock
		| M::DepletedStone
		| M::DepletedEmberore
		| M::DepletedDuskstone
		| M::DepletedGlowvein
		| M::DepletedAetherock
		| M::Heartwood
		| M::DrainedHeartwood
		| M::Leystone
		| M::SpentLeystone
		| M::Springstone
		| M::SpentSpringstone => return None,
	})
}

/// The Alchemy level for a FUNCTIONAL item — one §7.3 prices by hand
/// because it is a function rather than a substance. `None` = not a
/// functional item (ask the grid instead).
///
/// The twin of [`alchemy::functional_ideal`]: a level and a price are one
/// decision, pinned so neither can exist without the other.
pub fn functional(b: BlockType) -> Option<u32> {
	Some(match b {
		// The first vessel (§13.5): the bucket sits at the level everyone
		// starts at, so its PRICE is the gate — the opening dig-and-extract
		// loop pays for it, and "make your first vessel" is the game's
		// first craft beat rather than a level-up.
		BlockType::Bucket => 1,
		// First light, pre-ore on purpose (§7.3) — and past the leystone
		// reveal, since its price names Terra and Terra is a leystone draw
		// now.
		BlockType::Torch => 10,
		// THE essence tool (the redesign made it the skill's identity):
		// every essence in the game is a siphon draw, so the siphon sits
		// at the level everyone starts at, gated by its feedstock price
		// alone — "make your siphon" is the game's first craft beat. Its
		// price may never name an essence, the no-Ferrum deadlock rule
		// generalized to the whole currency.
		BlockType::EssenceSiphon => 1,
		// Furniture: the first things you make because you want them
		// rather than because you need them, so they sit just past the
		// timber set that supplies them.
		BlockType::Chair => 14,
		BlockType::Table => 14,
		// The bed waits for the Unda economy it costs, which is the whole
		// reason its price crosses families.
		BlockType::Bed => 24,
		// The pick: crafted here, usable at the near-matching MINING level
		// (`pick_cells`) — two skills, one item, and the reason Ferrum is
		// worth mining at all.
		BlockType::ResonantPick => 36,
		// The lamps: the first Ferrum sink outside tools, and the standing
		// Aether one — a repeatable per-lamp sink is what keeps Aetherock
		// worth mining late, where the pick and the sigil are both
		// one-offs.
		BlockType::Lantern => 28,
		BlockType::Aetherlamp => 50,
		// A pure Aether price for a thing that does nothing: the point IS
		// that it is the last line.
		BlockType::MastersSigil => 99,
		_ => return None,
	})
}

/// Alchemy level that unlocks crafting `b`; `None` = never craftable.
///
/// **Nothing the world hands you may be transmuted.** The bench exists to make
/// what a fresh world does not give away, so the catalog opens on
/// [`BlockType::is_bench_only`] before it asks any table — dirt, sand, snow,
/// grass, mud, the felled log and the bucket charge are things you go and pick
/// up, and a recipe for one is the bench competing with a shovel. What remains
/// is the worked stuff.
///
/// Clean rock is the exception the rule is BUILT to allow rather than one
/// carved out of it: the world is made of stone but hands you Rubble, so it is
/// bench-only by the same derivation as everything else.
///
/// Then §7.2's grid rule, enforced rather than described: a material × shape
/// pair unlocks at `max(shape level, material level)`, so reaching Stone opens
/// clean stone in block, slab, stairs and door forms together.
pub fn craft(b: BlockType) -> Option<u32> {
	if !b.is_bench_only() {
		return None;
	}
	if let Some(level) = functional(b) {
		return Some(level);
	}
	let (m, s) = b.grid_cell()?;
	Some(material(m)?.max(shape(s)))
}

/// The Mining level a vein of `b` becomes VISIBLE at (SKILLS-DESIGN
/// §5.3), or `None` for anything that is not an ore.
///
/// This is a stranger gate than the others, and deliberately: below it the
/// cell is Stone to that player in every respect — it renders as stone,
/// raycasts as stone, digs as stone, and the server agrees, so a
/// low-level miner tunnelling through a deposit spends it without ever
/// knowing. You dig what you can see. That is what makes crossing a
/// threshold an event (veins fade in around you) rather than a number.
pub fn reveal(b: BlockType) -> Option<u32> {
	Some(match b {
		// Deliberately ABOVE the Rock family's strength wall (§5.1), not on
		// it. Ore that reveals at exactly the level rock opens is ore you
		// have never not seen: the first seam would appear in the first
		// stone you were ever able to break, and the stretch of plain
		// quarrying the ladder is built around would not exist. It also
		// leaves the property untestable — an unrevealed vein digs as
		// ordinary stone, and there is no level that can dig stone and not
		// see the very first vein in it.
		BlockType::Emberore => 15,
		BlockType::Duskstone => 23,
		BlockType::Glowvein => 33,
		BlockType::Aetherock => 50,
		_ => return None,
	})
}

/// The ALCHEMY level a SURFACE node becomes visible at — [`reveal`]'s
/// twin on the other skill, so Alchemy owns the surface hunt the way
/// Mining owns the deep one (the essence redesign). Same stranger-gate
/// semantics: below it the cell is its host in every respect, and you
/// draw what you can see.
///
/// A node answers to exactly ONE of the two reveal tables (pinned in
/// `worldgen::nodes`).
pub fn alchemy_reveal(b: BlockType) -> Option<u32> {
	Some(match b {
		// The day-one node, and necessarily so: every essence in the game
		// now starts as a draw, so the first node must be visible at the
		// level everyone starts at or the economy cannot begin. The craft
		// beat it feeds is the siphon, whose feedstock-only price is what
		// actually gates the first draw.
		BlockType::Heartwood => 1,
		// The Terra node — early, but past the first heartwood draws, so
		// the surface ladder has an actual first reveal event.
		BlockType::Leystone => 5,
		// The Unda node, under water on purpose: reaching it is the trip.
		BlockType::Springstone => 12,
		_ => return None,
	})
}

/// Where the bench first quickens. A level that changes a NUMBER rather
/// than opening a recipe, so [`opens`] deliberately does not report it —
/// but it is still a level, and every level lives here. `alchemy` owns
/// what it steps TO ([`crate::alchemy::base_pace_ticks`]).
pub const BENCH_FASTER_AT: u32 = 25;

/// Where the bench reaches its fastest pace.
pub const BENCH_FASTEST_AT: u32 = 45;

/// Where transmutation waste steps down — just past where the catalog tops
/// out, so the post-content stretch has a felt gain of its own
/// ([`crate::alchemy::waste_permille`] owns the floor it clamps to).
pub const WASTE_DIP_AT: u32 = 56;

/// How many cells one hold breaks with the Resonant Pick in hand
/// (SKILLS-DESIGN §5.6): a single cell below the tool's own Mining level,
/// then a row, then the full square plane.
///
/// A width, not a boolean, because the pick's two ranks are one
/// progression: the same machinery widens, and the timing multiplier
/// beside it in `skills` steps with it.
pub fn pick_cells(level: u32) -> usize {
	match level {
		l if l >= PICK_PLANE_AT => PICK_PLANE,
		l if l >= PICK_WIDE_AT => 3,
		_ => 1,
	}
}

/// The Mining level the swing widens to a row at.
///
/// Named rather than left inside [`pick_cells`]'s match, and that is the
/// rule for every threshold in this file: one nothing can NAME is one
/// every reader writes down again — the character screen's teasers and
/// two test suites were each carrying their own copy.
pub const PICK_WIDE_AT: u32 = 33;

/// The Mining level the swing reaches [`PICK_PLANE`] at. Deep in the
/// post-catalog tail ON PURPOSE — the one functional unlock out there, a
/// long-haul goal past where the content ladder ends (SKILLS-DESIGN: the
/// 1–60 compression keeps exactly this exception).
pub const PICK_PLANE_AT: u32 = 85;

/// The pick's second rank: the full plane, the widest it ever breaks.
///
/// Named because two things key on "the pick is at its last rank" and
/// neither may own a threshold of its own — [`pick_cells`] here, and
/// [`crate::gi::carried_light`], which brightens the tool's glow at
/// exactly the level the swing widens.
pub const PICK_PLANE: usize = 9;

/// The **prospector's sense** (SKILLS-DESIGN §5.5): within
/// [`prospect_range`] of a deposit you cannot yet SEE, a faint shimmer.
/// Audio only, in the `Footsteps` cadence mould — never an overlay, or
/// the reveal levels would stop meaning anything.
pub fn prospector(level: u32) -> bool {
	prospect_range(level).is_some()
}

/// How far the prospector's sense reaches, in blocks — `None` before the
/// sense arrives at all. Two ranks, like the pick: it comes in faint just
/// past the first reveal (teasing the tier you cannot yet see), and
/// sharpens in the stretch before the deepest one — a manufactured
/// near-goal for exactly the levels with no threshold otherwise in sight.
pub fn prospect_range(level: u32) -> Option<i32> {
	match level {
		l if l >= PROSPECT_SHARP_AT => Some(8),
		l if l >= PROSPECT_FAINT_AT => Some(4),
		_ => None,
	}
}

/// Where the sense comes in faint — just past the first reveal, so it
/// teases the tier you cannot yet see.
pub const PROSPECT_FAINT_AT: u32 = 18;

/// Where it sharpens: the stretch before the deepest reveal, which has no
/// other threshold in sight.
pub const PROSPECT_SHARP_AT: u32 = 43;

/// The Mining mastery capstone (§5.5): every field extraction draws one
/// extra vein's worth of essence. It crowns the CONTENT ladder — the
/// level the last reveal's stretch ends at — rather than the level cap,
/// which the 1–60 compression leaves as expansion headroom.
///
/// Extra DRAWS rather than extra units, which is what `alchemy::field_yield`
/// spends it as: a tier that pays in two essences has to get more of both,
/// or the capstone would quietly favour whichever line was listed first.
pub fn ore_mastery(level: u32) -> u32 {
	u32::from(level >= MASTERY_AT)
}

/// Where mastery lands: the level the last reveal's stretch ends at, which
/// is the top of the CONTENT ladder rather than the level cap.
pub const MASTERY_AT: u32 = 60;

/// Arcana's home cooldown: how long Return Home rests between casts,
/// stepping DOWN at the two swift-return rungs — a rung is felt as "home
/// comes back sooner". The first level's answer IS the game's base
/// return cooldown; `digblock-net` aliases that base onto the protocol
/// const its message doc names, so there is one number.
pub fn return_cooldown_secs(level: u32) -> u32 {
	match level {
		l if l >= RETURN_SWIFTER_AT => 30,
		l if l >= RETURN_SWIFT_AT => 45,
		_ => RETURN_COOLDOWN_BASE_SECS,
	}
}

/// The cooldown every player starts at — and the one pre-Arcana players
/// effectively keep, since a fresh skill sits at the first level.
pub const RETURN_COOLDOWN_BASE_SECS: u32 = 60;

/// The first swift-return rung: the stretch between the second waystone
/// and the third otherwise has no threshold in sight.
pub const RETURN_SWIFT_AT: u32 = 18;

/// The second, deep in the ladder beside the last ports.
pub const RETURN_SWIFTER_AT: u32 = 56;

/// The waystone ladder: the Arcana level that makes each biome's port
/// castable, ordered by remoteness and rarity — common lowland first,
/// the far cold last, crowning the content top. `None` is a kind with no
/// port: the water kinds are not destinations, and a shore band or a
/// slope patch is scenery, not a place (the doc comment on the arm is
/// the roster's record). The sites themselves derive in
/// `waystones::anchor`; this table is the only place their levels live.
pub fn waystone(kind: crate::worldgen::SurfaceKind) -> Option<u32> {
	use crate::worldgen::SurfaceKind as K;
	Some(match kind {
		K::Plains => 6,
		K::Forest => 14,
		K::Savanna => 22,
		K::Taiga => 34,
		K::Beach => 42,
		K::Mountain => 50,
		K::Snow => 60,
		// Not destinations: the water kinds, and the two narrow bands.
		K::Ocean | K::River | K::Lake | K::Marsh | K::GravelShore | K::Scree => return None,
	})
}

/// The cast wind-up, in sim ticks — every spell channels through this,
/// so the quickening rung is felt on the whole spellbook at once.
pub fn channel_ticks(level: u32) -> u32 {
	if level >= CHANNEL_QUICK_AT { 72 } else { 120 }
}

/// Where every channel quickens — the ladder's midpoint breather between
/// the arc's first rank and the fourth waystone.
pub const CHANNEL_QUICK_AT: u32 = 30;

/// Where Kindle opens — conjured light in the world, a few levels past
/// the first port so the illumination family paces the travel one.
pub const KINDLE_AT: u32 = 10;

/// Where Kindle's fare eases, deep beside the last ports.
pub const KINDLE_CHEAP_AT: u32 = 54;

/// Wisp's second rank: brighter AND whitened — level alone reads as
/// more of the same hue, never as brighter (the carried-light lesson).
pub const WISP_BRIGHT_AT: u32 = 38;

/// The wisp's cast light for a caster at this Arcana: GI level and
/// whitening. The self-light answers to its CASTER, the pick's rule.
pub fn wisp_light(level: u32) -> (u8, bool) {
	if level >= WISP_BRIGHT_AT {
		(12, true)
	} else {
		(8, false)
	}
}

/// Seconds a wisp burns, doubling at the second rank.
pub fn wisp_secs(level: u32) -> u32 {
	if level >= WISP_BRIGHT_AT { 360 } else { 180 }
}

// There is deliberately NO per-substance draw gate. The reveal IS the
// gate: a node you cannot see draws as its host, and one you can needs
// only the siphon — a second level axis on top of the reveal would be
// two gates telling one story. (The extraction-level axis this note once
// defended died twice over: first to a single-entry table, then with
// bench extraction itself.)

/// §5.1's strength wall: the Mining level below which `b` refuses to be
/// dug at all; the first level means ungated. Deliberately a wall — the doc's original "no
/// strength wall" stance was reversed by design.
///
/// TWO gates, and a block waits for the later of them. [`mine_family`] is
/// the strength ladder every diggable block sits on; [`mine_material`] is
/// the handful of substances that additionally have to be EARNED, and it
/// can only ever push a block later, never earlier.
pub fn mine(b: BlockType) -> u32 {
	let family = mine_family(b.dig_family());
	let material = b.material().map_or(1, mine_material);
	family.max(material)
}

/// The strength wall keyed on the DIG FAMILY — the same axis that decides
/// how long a hold takes (§5.1), which is the only thing raw strength can
/// honestly be about. Soil and foliage from the first swing, then timber,
/// then rock: the ladder a player climbs is soil → trees → stone, and each
/// rung is opened by digging the one below it.
///
/// Keying it here rather than on the substance is what stopped rock being
/// the only family with a wall at all. It also puts GRAVEL behind the
/// same wall as stone — it used to be the ungated "entry rock", and it
/// isn't one any more, because timber is the thing that now stands
/// between bare ground and the quarry.
fn mine_family(f: crate::block::DigFamily) -> u32 {
	use crate::block::DigFamily as F;
	match f {
		F::Fixed | F::Soft => 1,
		F::Wood => 3,
		F::Rock => 10,
	}
}

/// The substances that have to be earned on top of their family's wall,
/// keyed on the SUBSTANCE because that is what being earned is about — a
/// metal stair resists exactly as its metal does, and listing every shape
/// by hand was how the old table came to name four blocks and miss the
/// fifth. Everything whose only wall is its strength answers the first
/// level here and takes [`mine_family`]'s number.
fn mine_material(m: Material) -> u32 {
	use Material as M;
	match m {
		// The ORES, at exactly the level that reveals them. A vein in
		// pristine stone is already invisible below its tier, so this arm is
		// about the other way an ore block exists: one somebody PLACED. That
		// is ordinary chunk data every visitor sees at any level, so without
		// these arms the table fell through to the ungated arm and a
		// brand-new player could strip a late-game player's Aetherock wall.
		// The family wall does not cover it: ore is Rock, so on strength
		// alone it would open with the rest of the rock.
		//
		// Keyed to `reveal` rather than hand-numbered, so a retune of the
		// reveal ladder cannot leave the two disagreeing
		// (`every_ore_is_gated_at_its_own_reveal`).
		M::Emberore => reveal_of(BlockType::Emberore),
		M::Duskstone => reveal_of(BlockType::Duskstone),
		M::Glowvein => reveal_of(BlockType::Glowvein),
		M::Aetherock => reveal_of(BlockType::Aetherock),
		// The hard substances ore pays for, at exactly the Alchemy level
		// that makes each one EXIST (`material`). A strength gate below the
		// craft level is provably dead weight — `pickup` already refuses the
		// drop until the recipe opens — and its fanfare celebrates a block
		// the player cannot obtain; a gate above it would let the mason be
		// walled out of their own build. So the two walls are one number,
		// in both hands, and the pin below holds them together.
		M::Metal => 33,
		M::Crystal => 40,
		// Everything else waits only for its family's strength wall.
		_ => 1,
	}
}

/// [`reveal`] for a block known to be an ore, for use inside
/// [`mine_material`]'s table. Panics on anything else, which is a
/// compile-time-shaped mistake made loud at first call rather than
/// silently answering as if ungated — the exact failure this helper exists
/// to fix.
fn reveal_of(b: BlockType) -> u32 {
	match reveal(b) {
		Some(l) => l,
		None => panic!("{b:?} is in the ore gate but has no reveal level"),
	}
}

/// The strength wall's Alchemy counterpart: the level below which `b` refuses
/// to be dug because picking it up would hand you an item you cannot make (the
/// first level means ungated).
///
/// Derived from [`craft`] rather than tabled again. It keys on the DROP, not
/// the block, because the gate is about the item that lands in the inventory —
/// mined rock is Rubble whatever the cell was, so quarrying pristine Stone
/// stays a Mining question. And it keys on `block::crafted` rather than on
/// craftability alone: `Wood` and `Lumber` are both craftable, but only one
/// grows on a tree. Craft-ONLY blocks are the ones a player can only have
/// crafted, so requiring their own recipe level is what stops anyone mining
/// around an unlock through someone else's build.
pub fn pickup(b: BlockType) -> u32 {
	match b.dig_drop() {
		Some(d) if d.is_crafted_only() => craft(d).unwrap_or(1),
		_ => 1,
	}
}

// ---- the questions ---------------------------------------------------

/// May a player at `level` transmute `b` at all? (False for anything with
/// no recipe, which is what makes `craft_ideal`'s `expect` sound.)
pub fn can_craft(b: BlockType, level: u32) -> bool {
	craft(b).is_some_and(|need| level >= need)
}

/// Which gate a dig ran into — the two read differently to a player, and
/// each carries the wording the feed speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
	/// The block is too hard for this Mining level (the strength wall).
	Strength,
	/// The block is craft-only and this Alchemy level cannot make one, so
	/// picking it up would hand out an item the player can't own yet.
	Recipe,
	/// A revealed ore vein and the wrong tool in hand (§5.3). Unlike the
	/// other two this is not about a LEVEL at all — it is the one refusal
	/// a player can clear without gaining anything, by switching tools,
	/// and the one that never appears in a [`Requirement`].
	Tool,
}

/// One thing a dig needs before it may start, and the block it is about —
/// carried so a refusal can name what it refused without a caller handing
/// back a block it might get wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Requirement {
	pub block: BlockType,
	pub skill: Skill,
	/// The level `skill` must have reached; the first level means no
	/// requirement at all.
	pub level: u32,
	pub kind: Kind,
}

/// Everything digging `b` asks for, in refusal priority order. Level 1
/// entries are satisfied by definition — they are kept in the array so
/// callers that GRANT levels (the test harness) can walk one list.
pub fn dig_requirements(b: BlockType) -> [Requirement; 2] {
	[
		Requirement {
			block: b,
			skill: Skill::Mining,
			level: mine(b),
			kind: Kind::Strength,
		},
		Requirement {
			block: b,
			skill: Skill::Alchemy,
			level: pickup(b),
			kind: Kind::Recipe,
		},
	]
}

/// THE dig-permission question: the first requirement `skills` fails, or
/// `None` when the block may be dug.
///
/// Every consumer of the gates routes through this — client prediction
/// (`diggable`, and the feed line `step_dig` speaks), the server's
/// `DigStart` backstop, the bot's target filter — so a new gate is added
/// in exactly one place and every one of them enforces it.
pub fn dig_blocked(b: BlockType, skills: &SkillSet) -> Option<Requirement> {
	dig_requirements(b)
		.into_iter()
		.find(|r| skills.level(r.skill) < r.level)
}

/// The requirement of this KIND that `b` actually carries, if any — how a
/// verdict that names a GATE (all the wire carries) becomes the sentence
/// prediction would have spoken. `None` where the block has no such gate,
/// which is the one case a listener has nothing of its own to say.
pub fn requirement(b: BlockType, kind: Kind) -> Option<Requirement> {
	dig_requirements(b)
		.into_iter()
		.find(|r| r.kind == kind && r.level > 1)
}

// ---- what a level opens ----------------------------------------------

/// Does reaching `level` in `skill` open something that was shut at
/// `level - 1`? The level-up fanfare asks this to pick its voice (a
/// bigger sound for a level that hands you something) and the character
/// screen's teasers are pinned against it, and the answer is DERIVED from
/// the tables themselves rather than a second list — retuning a threshold
/// moves the fanfare with it, and cannot drift.
///
/// The first level is never an unlock: it is where everyone starts, and no one
/// levels up INTO it.
pub fn opens(skill: Skill, level: u32) -> bool {
	if level <= 1 || level > MAX_LEVEL {
		return false;
	}
	BlockType::ALL.iter().any(|&b| match skill {
		// A rock whose strength wall stands exactly here. The craft gate
		// needs no arm of its own: it derives from `craft`, which the
		// Alchemy arm already covers.
		Skill::Mining => {
			mine(b) == level
				|| reveal(b) == Some(level)
				// The pick's two ranks — a level that hands you a wider
				// swing hands you something, exactly like a reveal.
				|| (b == BlockType::ResonantPick && pick_cells(level) != pick_cells(level - 1))
				// And the two endgame lines, which hand you something
				// without handing you an item.
				|| (b == BlockType::Air
					&& (prospect_range(level) != prospect_range(level - 1)
						|| ore_mastery(level) != ore_mastery(level - 1)))
		}
		// A new recipe, or a surface node fading into view.
		Skill::Alchemy => craft(b) == Some(level) || alchemy_reveal(b) == Some(level),
		// A waystone humming into reach, or a travel rung — the home
		// cooldown stepping down, the channel quickening: levels that
		// hand you something without handing you an item, on the
		// prospector/mastery pattern.
		Skill::Arcana => {
			b == BlockType::Air
				&& (crate::worldgen::SurfaceKind::ALL
					.iter()
					.any(|&k| waystone(k) == Some(level))
					|| level == KINDLE_AT
					|| return_cooldown_secs(level) != return_cooldown_secs(level - 1)
					|| channel_ticks(level) != channel_ticks(level - 1)
					|| wisp_light(level) != wisp_light(level - 1)
					|| crate::alchemy::kindle_price(level)
						!= crate::alchemy::kindle_price(level - 1))
		}
	})
}

#[cfg(test)]
mod tests;
