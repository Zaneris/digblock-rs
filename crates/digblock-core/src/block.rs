//! Block types and their data-driven properties. Adding a block is: one
//! `blocks!` row, a texture PNG, and a `LAYERS` entry in digblock-assets
//! — nothing else.
//!
//! Values are contiguous so they index tables directly (the Java used
//! AIR=-1, WATER=0, DIRT=1, GRASS=2; shifted by +1 here so a chunk is a
//! flat array of cells with AIR=0 as the natural default).

use crate::dir::Dir;
use crate::material::Material;

/// One table generates the enum, `COUNT`, `ALL`, and `PROPS` together —
/// adding a block is ONE row here (plus a PNG + `LAYERS` entry in
/// digblock-assets); the old hand-synced quadruple cannot skew.
macro_rules! blocks {
	(
		$(#[$fmeta:meta])* $first:ident = $fid:literal => $fprops:expr,
		$($(#[$meta:meta])* $name:ident = $id:literal => $props:expr,)+
	) => {
		#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
		#[repr(u16)]
		pub enum BlockType {
			$(#[$fmeta])*
			#[default]
			$first = $fid,
			$($(#[$meta])* $name = $id,)+
		}

		impl BlockType {
			pub const COUNT: usize = 1 + [$($id as u16),+].len();
			pub const ALL: [BlockType; Self::COUNT] =
				[BlockType::$first, $(BlockType::$name),+];
		}

		/// Indexed by `BlockType as usize` — discriminant contiguity is
		/// pinned by `discriminants_index_props`.
		pub const PROPS: [BlockProps; BlockType::COUNT] = [$fprops, $($props),+];
	};
}

blocks! {
	Air = 0 => props(RenderClass::None, false, 0, 0, 0),
	/// A water source: infinite, amount 8. All generated water (oceans,
	/// river pools) is sources — flowing/falling states describe water in
	/// transit under the Minecraft-style flow rules. Diggable only via the
	/// water tool (the fluid-aware ray); the hardness is the scoop time.
	Water = 1 => dig(water_props(8, true, false), 20, DigFamily::Fixed),
	Dirt = 2 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::DIRT, tex::DIRT, tex::DIRT)), 30, DigFamily::Soft), Material::Dirt),
	Grass = 3 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::GRASS_SIDE, tex::GRASS_TOP, tex::DIRT)), 30, DigFamily::Soft), Material::Grass),
	Sand = 4 => mat(dig(props(RenderClass::Opaque, true, tex::SAND, tex::SAND, tex::SAND), 25, DigFamily::Soft), Material::Sand),
	/// Mined stone drops Rubble (SKILLS-DESIGN §7.1): the clean block
	/// exists only pristine in terrain or transmuted at the bench, which
	/// is what makes transmutation the economy rather than a curiosity.
	Stone = 5 => mat(drops(dig(carveable(props(RenderClass::Opaque, true, tex::STONE, tex::STONE, tex::STONE)), 90, DigFamily::Rock), BlockType::Rubble), Material::Stone),
	Wood = 6 => mat(dig(props(RenderClass::Opaque, true, tex::LOG, tex::LOG_TOP, tex::LOG_TOP), 60, DigFamily::Wood), Material::Wood),
	Leaves = 7 => mat(dig(props(RenderClass::Cutout, true, tex::LEAVES, tex::LEAVES, tex::LEAVES), 10, DigFamily::Fixed), Material::Leaf),
	Snow = 8 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::SNOW, tex::SNOW, tex::SNOW)), 15, DigFamily::Soft), Material::Snow),
	/// Flowing water, amount 1 (thinnest) … 7. Spread sideways from a
	/// source or a landed fall, losing one amount per block.
	WaterFlow1 = 9 => water_props(1, false, false),
	WaterFlow2 = 10 => water_props(2, false, false),
	WaterFlow3 = 11 => water_props(3, false, false),
	WaterFlow4 = 12 => water_props(4, false, false),
	WaterFlow5 = 13 => water_props(5, false, false),
	WaterFlow6 = 14 => water_props(6, false, false),
	WaterFlow7 = 15 => water_props(7, false, false),
	/// Water falling through air: renders full-height, lands at amount 8.
	WaterFall = 16 => water_props(8, false, true),
	/// Savanna surface: sun-dried grass over dirt.
	DryGrass = 17 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::DRY_GRASS_SIDE, tex::DRY_GRASS_TOP, tex::DIRT)), 30, DigFamily::Soft), Material::DryGrass),
	/// Marsh surface and subsoil: wet dark earth.
	Mud = 18 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::MUD, tex::MUD, tex::MUD)), 25, DigFamily::Soft), Material::Mud),
	/// Steep-shore and scree surface: loose pebbled rock.
	Gravel = 19 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::GRAVEL, tex::GRAVEL, tex::GRAVEL)), 45, DigFamily::Rock), Material::Gravel),
	/// Conifer foliage; a structure block like Leaves, so not carveable.
	PineLeaves = 20 => mat(dig(props(RenderClass::Cutout, true, tex::PINE_LEAVES, tex::PINE_LEAVES, tex::PINE_LEAVES), 10, DigFamily::Fixed), Material::PineLeaf),
	/// The stratum under sandy floors (ocean, beach, river, lake beds).
	/// "All broken rock": mined sandstone is Rubble too, so the clean
	/// block is craft-only like Stone's.
	Sandstone = 21 => mat(drops(dig(carveable(props(RenderClass::Opaque, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), 70, DigFamily::Rock), BlockType::Rubble), Material::Sandstone),
	/// 3-step stairs (12ths sub-grid; facing in the cell state nibble).
	/// Crafted via alchemy from sub-wood, never generated.
	WoodStairs = 22 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LOG, tex::LOG_TOP, tex::LOG_TOP)), ShapeKind::Stairs), 60, DigFamily::Wood), Material::Wood),
	StoneStairs = 23 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE)), ShapeKind::Stairs), 90, DigFamily::Rock), Material::Stone),
	/// Two-cell door (facing/open/half in the state nibble): thin leaf,
	/// divided-lite window in the upper half. Crafted via alchemy — out
	/// of planed `Lumber` like the rest of the joinery, which is why it
	/// wears LUMBER_LIGHT and unlocks with the other lumber shapes.
	LumberDoor = 24 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Door), 45, DigFamily::Wood), Material::Lumber),
	/// Stackable bottom-anchored third-slab (state bit 0 = two-thirds).
	/// Placing onto a slab's top face grows it; the third grow converts
	/// the cell to the plain base block. Hardness is a third of the base
	/// block's, since each dig removes one third. Crafted via alchemy.
	WoodSlab = 25 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LOG, tex::LOG_TOP, tex::LOG_TOP)), ShapeKind::Slab), 20, DigFamily::Wood), Material::Wood),
	StoneSlab = 26 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE)), ShapeKind::Slab), 30, DigFamily::Rock), Material::Stone),
	/// What mined rock actually is (SKILLS-DESIGN §7.1): cracked, fully
	/// placeable, and the mass Earth feedstock at the bench. Placed, it
	/// reads as rough cobble — the free rustic building material. Drops
	/// itself, so it round-trips.
	Rubble = 27 => mat(dig(props(RenderClass::Opaque, true, tex::RUBBLE, tex::RUBBLE, tex::RUBBLE), 90, DigFamily::Rock), Material::Rubble),
	/// First light (SKILLS-DESIGN §7.3/§7.4): a floor-mounted post with a
	/// glowing head, crafted at Alch 8 — before ore on purpose. Stage-A
	/// emissive (fullbright ink) AND a Stage-B source: it seeds level 14
	/// into the voxel light field (LIGHT-DESIGN.md).
	Torch = 28 => dig(emits(glows(shaped(crafted(props(RenderClass::Shaped, true, tex::TORCH_SIDE, tex::TORCH_FLAME, tex::TORCH_SIDE)), ShapeKind::Torch)), 14), 10, DigFamily::Fixed),
	/// The water tool as an ITEM-ONLY row (SKILLS-DESIGN §6): tools are
	/// blocks that never render, collide, place, or slot — they exist so
	/// ownership, prediction, rebase, and both persistence formats ride
	/// the count machinery untouched. The bucket is owned by everyone
	/// always ([`BlockType::always_owned`]); its charge stays the Water
	/// count. Texture layers are 0 (never sampled — RenderClass::None).
	Bucket = 29 => props(RenderClass::None, false, 0, 0, 0),
	/// Planed timber (SKILLS-DESIGN §7.2's material × shape grid): the
	/// refined counterpart of the felled `Wood` trunk, craft-only and
	/// never generated — the bench's second timber, opened (Alch 15) after
	/// the log's own shapes. Exists as a full block because its slab has to
	/// have something to complete into (three thirds = one block, the
	/// rule that closes the slab economy).
	Lumber = 30 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), 60, DigFamily::Wood), Material::Lumber),
	LumberStairs = 31 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Stairs), 60, DigFamily::Wood), Material::Lumber),
	LumberSlab = 32 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Slab), 20, DigFamily::Wood), Material::Lumber),
	/// The mason's door — §7.2's grid at work rather than a curated row: it
	/// lands at Alch 20 because clean Stone does, and it arrives WITH the
	/// stone block, slab and stairs because those shapes were learned long
	/// before. Hardness is the timber door's 0.75× of its own base block.
	StoneDoor = 33 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE)), ShapeKind::Door), 68, DigFamily::Rock), Material::Stone),
	/// Sandstone's set, likewise free: the material opens at 30 and hands
	/// over every shape at once.
	SandstoneStairs = 34 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), ShapeKind::Stairs), 70, DigFamily::Rock), Material::Sandstone),
	SandstoneSlab = 35 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), ShapeKind::Slab), 23, DigFamily::Rock), Material::Sandstone),
	SandstoneDoor = 36 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), ShapeKind::Door), 53, DigFamily::Rock), Material::Sandstone),
	/// The second timber (Alch 25). `LUMBER_DARK` was drawn and registered
	/// with `LUMBER_LIGHT` and then sat unused on no block for a release —
	/// under the grid a whole new joinery set is four rows and no new art.
	DarkLumber = 37 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), 60, DigFamily::Wood), Material::DarkLumber),
	DarkLumberStairs = 38 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), ShapeKind::Stairs), 60, DigFamily::Wood), Material::DarkLumber),
	DarkLumberSlab = 39 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), ShapeKind::Slab), 20, DigFamily::Wood), Material::DarkLumber),
	DarkLumberDoor = 40 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), ShapeKind::Door), 45, DigFamily::Wood), Material::DarkLumber),
	/// The third timber (Alch 40).
	RedLumber = 41 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::LUMBER_RED, tex::LUMBER_RED, tex::LUMBER_RED)), 60, DigFamily::Wood), Material::RedLumber),
	RedLumberStairs = 42 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_RED, tex::LUMBER_RED, tex::LUMBER_RED)), ShapeKind::Stairs), 60, DigFamily::Wood), Material::RedLumber),
	RedLumberSlab = 43 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_RED, tex::LUMBER_RED, tex::LUMBER_RED)), ShapeKind::Slab), 20, DigFamily::Wood), Material::RedLumber),
	RedLumberDoor = 44 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_RED, tex::LUMBER_RED, tex::LUMBER_RED)), ShapeKind::Door), 45, DigFamily::Wood), Material::RedLumber),
	/// Furniture (SKILLS-DESIGN §7.3): FUNCTIONAL items, not §7.2 grid
	/// cells — a table is a thing, the way a torch is, rather than a form
	/// any substance could be worked into. So they carry no material,
	/// wear the joinery timber, and are priced by hand — which is also
	/// what lets the bed's cost cross essence families.
	Table = 45 => dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Table), 30, DigFamily::Wood),
	Chair = 46 => dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Chair), 25, DigFamily::Wood),
	Bed = 47 => dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Bed), 35, DigFamily::Wood),
	/// The four ores (SKILLS-DESIGN §5.3). These rows exist for the ITEM
	/// and for the reveal stamp; worldgen places none of them, because ore
	/// is DERIVED — `build_column` still returns Stone and the journal
	/// still carries Stone, which is what lets veins appear in worlds that
	/// already exist. Not `crafted` either: the bench cannot make ore, so
	/// gating its pickup would gate a thing with no recipe to gate it by.
	/// A PLACED ore block is ordinary chunk data — visible to everyone at
	/// any level, diggable at face value, and it drops itself, so showing
	/// off a find round-trips.
	Emberore = 48 => mat(dig(props(RenderClass::Opaque, true, tex::EMBERORE, tex::EMBERORE, tex::EMBERORE), 120, DigFamily::Rock), Material::Emberore),
	Duskstone = 49 => mat(dig(props(RenderClass::Opaque, true, tex::DUSKSTONE, tex::DUSKSTONE, tex::DUSKSTONE), 150, DigFamily::Rock), Material::Duskstone),
	/// The deep pair glow: Stage-A emissive INK only, no `emits` — a
	/// revealed seam glitters in a dark cave and casts nothing, which is
	/// the split the two light properties exist for.
	Glowvein = 50 => mat(glows(dig(props(RenderClass::Opaque, true, tex::GLOWVEIN, tex::GLOWVEIN, tex::GLOWVEIN), 190, DigFamily::Rock)), Material::Glowvein),
	Aetherock = 51 => mat(glows(dig(props(RenderClass::Opaque, true, tex::AETHEROCK, tex::AETHEROCK, tex::AETHEROCK), 240, DigFamily::Rock)), Material::Aetherock),
	/// The tapped-out vein face: what a field extraction leaves behind
	/// instead of a hole, so mining ore never excavates and the mountain
	/// keeps its shape. Crumbles to Rubble like any other rock.
	DepletedStone = 52 => mat(drops(dig(props(RenderClass::Opaque, true, tex::DEPLETED_STONE, tex::DEPLETED_STONE, tex::DEPLETED_STONE), 90, DigFamily::Rock), BlockType::Rubble), Material::DepletedStone),
}

/// The indestructible world floor: cells at this height can never be
/// dug (`BlockType::dig_hardness`), and worldgen's cave pass never
/// carves them — so no path exists to a hole in the bottom of the world.
pub const BEDROCK_Y: i32 = 0;

/// Ceiling on texture-array layers, tied to the packed vertex format's
/// layer field width. Every PROPS row is const-checked against it.
pub const MAX_TEX_LAYERS: usize = 1024;
const _: () = {
	let mut i = 0;
	while i < BlockType::COUNT {
		let p = &PROPS[i];
		assert!(
			(p.tex_side as usize) < MAX_TEX_LAYERS
				&& (p.tex_top as usize) < MAX_TEX_LAYERS
				&& (p.tex_bottom as usize) < MAX_TEX_LAYERS,
			"texture layer exceeds the vertex format's layer field"
		);
		assert!(p.light <= 15, "cast light exceeds the field's nibble");
		i += 1;
	}
};

/// Number of shaped block types — sizes the UI's 3D-icon vertex budget.
pub const SHAPED_COUNT: usize = {
	let mut n = 0;
	let mut i = 0;
	while i < BlockType::COUNT {
		if PROPS[i].shape.is_some() {
			n += 1;
		}
		i += 1;
	}
	n
};

/// How a block participates in meshing and rendering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenderClass {
	/// Emits no faces, occludes nothing (Air).
	None,
	/// Solid mesh; occludes neighbor faces.
	Opaque,
	/// Solid mesh; does not occlude; culls faces against same-type
	/// neighbors (leaves-style).
	Cutout,
	/// Water mesh (alpha-blended, no depth write); emits faces only toward
	/// `None` neighbors; culls same-type; top faces get the surface lip.
	Translucent,
	/// Sub-block geometry from the `shapes` registry (12ths grid). Solid
	/// stream; occlusion is per-face via the shape's coverage masks, and
	/// shaped blocks never cast AO (like Cutout).
	Shaped,
}

/// Which shape family a `RenderClass::Shaped` block renders/collides as.
/// The geometry itself (boxes, quads, masks) lives in `crate::shapes` —
/// this stays a plain tag so block.rs keeps sitting below the mesher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeKind {
	/// 3 steps at 4/12 risers; state nibble bits 0-1 = facing.
	Stairs,
	/// Thin two-cell leaf; state nibble = facing | open<<2 | upper<<3.
	Door,
	/// Bottom-anchored 12×12×4 (state 0) or 12×12×8 (state 1) fill.
	Slab,
	/// Floor-mounted torch: post + glowing head, one stateless variant.
	Torch,
	/// Full-footprint top on four legs; stateless, so tables pushed
	/// together read as one surface.
	Table,
	/// Seat, back and four legs; state nibble bits 0-1 = facing (the back).
	Chair,
	/// A one-cell cot: mattress on a low frame, pillow at the head end;
	/// state nibble bits 0-1 = facing (the head).
	Bed,
}

/// The SHAPE axis of SKILLS-DESIGN §7.2's material × shape grid: the form
/// a substance is worked into. Distinct from [`ShapeKind`], which is a
/// rendering/collision tag — `Block` has no sub-block geometry at all, and
/// `ShapeKind::Torch` is a functional item rather than a form you can put
/// any material into, so it has no family here.
///
/// The transmute tab's families ARE this enum, which is why a new
/// material × shape pair grows the catalog without growing the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeFamily {
	/// The full cube — the shape a material arrives in.
	Block,
	Slab,
	Stairs,
	Door,
}

impl ShapeFamily {
	pub const COUNT: usize = 4;
	/// The order the transmute tab lists families in — small worked pieces
	/// first, the full cube last, which is also roughly cheapest-first.
	/// There is exactly ONE such order: the screen reads this rather than
	/// keeping a display list of its own.
	pub const ALL: [ShapeFamily; Self::COUNT] = [
		ShapeFamily::Slab,
		ShapeFamily::Stairs,
		ShapeFamily::Door,
		ShapeFamily::Block,
	];

	/// The tab's family label (plural — it names a group of tiles).
	pub fn name(self) -> &'static str {
		match self {
			ShapeFamily::Block => "BLOCKS",
			ShapeFamily::Slab => "SLABS",
			ShapeFamily::Stairs => "STAIRS",
			ShapeFamily::Door => "DOORS",
		}
	}

	/// What an ITEM of this form is called after its material — "STONE" +
	/// "STAIRS". `None` for the full cube, which is just the substance's
	/// own name: nobody calls it a stone block.
	pub fn item_suffix(self) -> Option<&'static str> {
		match self {
			ShapeFamily::Block => None,
			ShapeFamily::Slab => Some("SLAB"),
			ShapeFamily::Stairs => Some("STAIRS"),
			ShapeFamily::Door => Some("DOOR"),
		}
	}

	/// The form's volume in sub-units (27ths of a block) — §4.2's volume
	/// rule, and the factor every craft cost and extraction yield in this
	/// material is prorated by. Pinned against the rasterized geometry by
	/// `shapes::craft_costs_derive_from_collision_volume`.
	pub fn units(self) -> u32 {
		match self {
			// A whole cube prorates to itself, which is what keeps every
			// raw-block recipe exactly what it was before the grid.
			ShapeFamily::Block => crate::shapes::SUBS_PER_BLOCK,
			// A third of a block: three slabs make one, the rule that
			// closes the grow-to-base-block economy.
			ShapeFamily::Slab => 9,
			ShapeFamily::Stairs => 18,
			// Both halves uncut (the window lites don't discount), plus the
			// handle and the two hinges.
			ShapeFamily::Door => 10,
		}
	}
}

/// Alchemy essence families — what blocks extract INTO at the bench
/// (SKILLS-DESIGN §3). `repr` indices are a wire and persistence format
/// (flagged inventory pairs `SUB_WIRE_FLAG | index`), APPEND-ONLY.
///
/// The first two indices deliberately keep the retired `Material`
/// enum's numbering — Flora sits where sub-wood (0) lived and Earth
/// where sub-stone (1) did — so every record and save written under the
/// old two-family economy decodes 1:1 into the right essence with NO
/// migration code anywhere. The design doc's decode-time migration is
/// this numbering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Essence {
	/// Living matter: wood, grass tops, leaves. Legacy sub-wood's slot.
	Flora = 0,
	/// Mineral bulk: stone, sand, dirt. Legacy sub-stone's slot.
	Earth = 1,
	Aqua = 2,
	/// Refined ore (bench-processed) — arrives with the ore milestone.
	Metal = 3,
	/// The rare one: cannot be transmuted into, ever.
	Aether = 4,
}

impl Essence {
	pub const COUNT: usize = 5;
	pub const ALL: [Essence; Self::COUNT] = [
		Essence::Flora,
		Essence::Earth,
		Essence::Aqua,
		Essence::Metal,
		Essence::Aether,
	];

	/// Strict decode of a wire family index.
	#[inline]
	pub fn try_from_index(v: u16) -> Option<Self> {
		Self::ALL.get(v as usize).copied()
	}

	/// The screens' all-caps label.
	pub fn name(self) -> &'static str {
		match self {
			Essence::Flora => "FLORA",
			Essence::Earth => "EARTH",
			Essence::Aqua => "AQUA",
			Essence::Metal => "METAL",
			Essence::Aether => "AETHER",
		}
	}
}

/// Which vertex stream a block's faces land in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MeshBucket {
	/// The single-word solid stream (opaque and cutout geometry).
	Solid,
	/// The two-word water stream (surface lips, flow/splash payload,
	/// alpha blend, no depth write).
	Water,
}

/// Fluid behavior, when a block IS one. Data, not variant identity: a
/// second fluid (lava) is new PROPS rows, not new match arms across the
/// mesher and the fluid sim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FluidProps {
	/// Level 1..=8 this block holds (sources and falls are full).
	pub amount: u8,
	/// Infinite, never recomputed by the fluid sim.
	pub source: bool,
	/// Falling through air; renders full-height, lands at full amount.
	pub falling: bool,
}

pub struct BlockProps {
	/// Occlusion/face-culling rule ONLY — bucket and fluid behavior are
	/// separate fields (a future glass is Translucent but Solid-bucket
	/// and not a fluid).
	pub class: RenderClass,
	/// Blocks the player and stops the picking ray.
	pub collides: bool,
	pub bucket: MeshBucket,
	pub fluid: Option<FluidProps>,
	/// Cave carving may replace this block (natural terrain, not
	/// structures) — consumed by worldgen's cave pass.
	pub carveable: bool,
	/// The bench is this block's ONLY source: worldgen never places one, so
	/// every one standing in a world was crafted by somebody. That is what
	/// lets `unlock::pickup` refuse the DIG below the block's own
	/// `unlock::craft` level — an item you cannot make yet is an item you
	/// cannot mine your way around either. Pinned against worldgen itself
	/// by `worldgen_never_places_a_crafted_block`.
	pub crafted: bool,
	/// Texture-array layer indices (order fixed by
	/// digblock_assets::tex::LAYERS). u16 so the full MAX_TEX_LAYERS
	/// range is reachable — the vertex format's layer field is 10 bits.
	pub tex_side: u16,
	pub tex_top: u16,
	pub tex_bottom: u16,
	/// Hold-to-dig duration in 60 Hz sim ticks; 0 = cannot be dug or
	/// collected. Shared client/server behavior — retuning is a
	/// SIM_VERSION bump.
	pub hardness_ticks: u16,
	/// Inventory credit override for a completed dig; `dig_drop()` falls
	/// back to the block itself when diggable.
	pub drops: Option<BlockType>,
	/// Sub-block shape family; `Some` ⟺ `class == RenderClass::Shaped`
	/// (pinned by `table_sanity`).
	pub shape: Option<ShapeKind>,
	/// Stage-A light (SKILLS-DESIGN §7.4): every face renders fullbright
	/// ink — the shader skips the sun/shadow/AO terms. Independent of
	/// [`Self::light`] on purpose: ore-reveal seams want ink that casts
	/// nothing, and a future recessed lamp could cast without glowing.
	pub emissive: bool,
	/// Stage-B cast light (LIGHT-DESIGN.md): the level this block SEEDS
	/// into the flood-fill field, 0–15. Purely client-side data — the
	/// field it feeds is derived, never serialized.
	pub light: u8,
	/// Which per-material dig-speed curve applies (SKILLS-DESIGN §5.1).
	/// Set together with hardness by the `dig` builder so a diggable row
	/// can never forget it. Shared client/server behavior — a family
	/// change is a SIM_VERSION bump like a hardness retune.
	pub dig_family: DigFamily,
	/// The substance this block is made OF (SKILLS-DESIGN §7.2's grid).
	/// Together with [`Self::shape`] it derives the unlock level, the craft
	/// cost, the extraction yield and the rock gate — see
	/// [`crate::material`]. `None` for air, the water family and every
	/// FUNCTIONAL item (tools, torches, lamps): function is priced by hand,
	/// never by volume, so those carry their own small tables instead.
	pub material: Option<Material>,
}

/// The per-material dig-speed families (SKILLS-DESIGN §5.1). Each keys a
/// Mining-level curve in `skills::dig_speed_permille`; all families pass
/// EXACTLY through 1000‰ at level 30 (the legacy table is the shared
/// mid-curve golden), so the family only shapes the approach and the
/// ceiling. `Fixed` is the flat curve for things that must never scale:
/// near-instant foliage, fixtures, and the water scoop whose duration is
/// pinned to the viewmodel swing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DigFamily {
	/// 1000‰ at every level — never faster, never slower.
	Fixed,
	/// Loose ground: today's original curve (1250 → 1000 → 417).
	Soft,
	/// Timber: a firmer start, a gentler ceiling (1350 → 1000 → 500).
	Wood,
	/// Rock resists a novice hard (1700 → 1000 → 417) and is level-gated
	/// besides (`unlock::mine`).
	Rock,
}

const fn props(class: RenderClass, collides: bool, side: u16, top: u16, bottom: u16) -> BlockProps {
	BlockProps {
		class,
		collides,
		bucket: MeshBucket::Solid,
		fluid: None,
		carveable: false,
		crafted: false,
		tex_side: side,
		tex_top: top,
		tex_bottom: bottom,
		hardness_ticks: 0,
		drops: None,
		shape: None,
		emissive: false,
		light: 0,
		dig_family: DigFamily::Fixed,
		material: None,
	}
}

/// A sub-block shaped row: tags the shape family the `shapes` registry
/// resolves geometry from.
const fn shaped(mut p: BlockProps, kind: ShapeKind) -> BlockProps {
	p.shape = Some(kind);
	p
}

/// Natural terrain the cave pass may carve through.
const fn carveable(mut p: BlockProps) -> BlockProps {
	p.carveable = true;
	p
}

/// The other half of `carveable`'s question — where a block comes from:
/// this one is never generated, only transmuted at the bench, which is
/// what gates digging it up (`unlock::pickup`).
const fn crafted(mut p: BlockProps) -> BlockProps {
	p.crafted = true;
	p
}

/// Diggable: hold-to-dig completes after `ticks` 60 Hz sim ticks at the
/// level-30 reference, scaled by `family`'s Mining curve. Family rides
/// the same builder as hardness so a new diggable row can't forget it.
const fn dig(mut p: BlockProps, ticks: u16, family: DigFamily) -> BlockProps {
	p.hardness_ticks = ticks;
	p.dig_family = family;
	p
}

/// Inventory credit override: a completed dig lands `d`, not the block
/// itself (the rock family's Rubble rule).
const fn drops(mut p: BlockProps, d: BlockType) -> BlockProps {
	p.drops = Some(d);
	p
}

/// The substance half of SKILLS-DESIGN §7.2's grid. Everything a
/// structural block's economy needs — unlock level, craft cost, extraction
/// yield, rock gate — falls out of this plus the row's `shaped` tag, so a
/// material × shape pair is a row and a PNG rather than four table edits.
/// FUNCTIONAL items deliberately have none (`material_roster_is_pinned`
/// holds the split).
const fn mat(mut p: BlockProps, m: Material) -> BlockProps {
	p.material = Some(m);
	p
}

/// Stage-A emissive: the block's faces render fullbright.
const fn glows(mut p: BlockProps) -> BlockProps {
	p.emissive = true;
	p
}

/// Stage-B cast light: this block seeds `level` (1–15) into the voxel
/// light field.
const fn emits(mut p: BlockProps, level: u8) -> BlockProps {
	p.light = level;
	p
}

/// A water-family row: water bucket + fluid behavior.
const fn water_props(amount: u8, source: bool, falling: bool) -> BlockProps {
	BlockProps {
		class: RenderClass::Translucent,
		collides: false,
		bucket: MeshBucket::Water,
		fluid: Some(FluidProps {
			amount,
			source,
			falling,
		}),
		carveable: false,
		crafted: false,
		tex_side: tex::WATER,
		tex_top: tex::WATER,
		tex_bottom: tex::WATER,
		hardness_ticks: 0,
		drops: None,
		shape: None,
		emissive: false,
		light: 0,
		dig_family: DigFamily::Fixed,
		material: None,
	}
}

/// Texture-array layers, matching digblock_assets::tex::LAYERS order.
pub mod tex {
	pub const WATER: u16 = 0;
	pub const DIRT: u16 = 1;
	pub const GRASS_SIDE: u16 = 2;
	pub const GRASS_TOP: u16 = 3;
	pub const SAND: u16 = 4;
	pub const STONE: u16 = 5;
	/// The felled trunk: bark on the sides, growth rings on the cut ends.
	pub const LOG: u16 = 6;
	pub const LOG_TOP: u16 = 7;
	pub const LEAVES: u16 = 8;
	pub const SNOW: u16 = 9;
	pub const DRY_GRASS_SIDE: u16 = 10;
	pub const DRY_GRASS_TOP: u16 = 11;
	pub const MUD: u16 = 12;
	pub const GRAVEL: u16 = 13;
	pub const PINE_LEAVES: u16 = 14;
	pub const SANDSTONE: u16 = 15;
	pub const RUBBLE: u16 = 16;
	pub const TORCH_SIDE: u16 = 17;
	pub const TORCH_FLAME: u16 = 18;
	/// Planed timber — the material crafted things are made OF, as
	/// opposed to [`LOG`], which is the tree. [`LUMBER_LIGHT`] is the
	/// joinery (the bucket, the torch post, and the Lumber block with its
	/// stairs, slab and door); `LUMBER_DARK` and `LUMBER_RED` are
	/// registered and unused — the obvious second and third timbers.
	pub const LUMBER_LIGHT: u16 = 19;
	pub const LUMBER_DARK: u16 = 20;
	pub const LUMBER_RED: u16 = 21;
	/// The four ore faces and the tapped one (SKILLS-DESIGN §5.3): stone
	/// bodies with tier-coloured flecks, the deep two bright enough to
	/// read as ink in a dark cave.
	pub const EMBERORE: u16 = 22;
	pub const DUSKSTONE: u16 = 23;
	pub const GLOWVEIN: u16 = 24;
	pub const AETHEROCK: u16 = 25;
	pub const DEPLETED_STONE: u16 = 26;

	/// Layers in the array, i.e. `digblock_assets::tex::LAYERS.len()`.
	/// Pinned against it by `every_block_texture_layer_is_uploaded` in
	/// digblock-render, the one crate that can see both.
	pub const COUNT: u16 = DEPLETED_STONE + 1;
}

impl BlockType {
	/// Forgiving decode for world data: out-of-range ids read as Air.
	/// The id occupies the low [`crate::chunk::ID_BITS`] of a cell; any
	/// state bits must be stripped by the caller (`ChunkData::get` does).
	#[inline]
	pub fn from_id(v: u16) -> Self {
		if (v as usize) < Self::COUNT {
			Self::ALL[v as usize]
		} else {
			BlockType::Air
		}
	}

	/// Strict decode for VALIDATION paths (network input): `None` for
	/// unknown ids or set state bits, instead of silently reading Air.
	#[inline]
	pub fn try_from_id(v: u16) -> Option<Self> {
		((v as usize) < Self::COUNT).then(|| Self::ALL[v as usize])
	}

	#[inline]
	pub fn props(self) -> &'static BlockProps {
		&PROPS[self as usize]
	}

	/// Texture-array layer for a face in direction `dir`.
	#[inline]
	pub fn texture(self, dir: Dir) -> u32 {
		let p = self.props();
		u32::from(match dir {
			Dir::Top => p.tex_top,
			Dir::Bottom => p.tex_bottom,
			_ => p.tex_side,
		})
	}

	/// Stage-B cast-light level (0 = casts nothing). The seed value the
	/// voxel light field's flood fill starts from (LIGHT-DESIGN.md).
	#[inline]
	pub fn light_level(self) -> u8 {
		self.props().light
	}

	/// The per-material dig-speed family (SKILLS-DESIGN §5.1).
	#[inline]
	pub fn dig_family(self) -> DigFamily {
		self.props().dig_family
	}

	/// The substance this block is made of — the material half of §7.2's
	/// grid. `None` for air, the water family, and functional items.
	#[inline]
	pub fn material(self) -> Option<Material> {
		self.props().material
	}

	/// The form this block is worked into — the shape half of §7.2's grid.
	/// `None` for a FUNCTIONAL shape (the torch), which is a thing rather
	/// than a form any material could take.
	#[inline]
	pub fn shape_family(self) -> Option<ShapeFamily> {
		match self.props().shape {
			None => Some(ShapeFamily::Block),
			Some(ShapeKind::Slab) => Some(ShapeFamily::Slab),
			Some(ShapeKind::Stairs) => Some(ShapeFamily::Stairs),
			Some(ShapeKind::Door) => Some(ShapeFamily::Door),
			// Functional shapes: things, not forms any substance could be
			// worked into. They opt out of the grid and are priced by hand.
			Some(ShapeKind::Torch | ShapeKind::Table | ShapeKind::Chair | ShapeKind::Bed) => None,
		}
	}

	/// Does placing this block derive a facing from the placer's look?
	/// The state encoding is shared (bits 0-1), so one predicate covers
	/// the ascending stair and every piece of furniture that has a front.
	#[inline]
	pub fn takes_facing(self) -> bool {
		matches!(
			self.props().shape,
			Some(ShapeKind::Stairs | ShapeKind::Chair | ShapeKind::Bed)
		)
	}

	/// Is this block one cell of §7.2's material × shape grid — a substance
	/// in a form — as opposed to a functional item priced by hand?
	#[inline]
	pub fn is_grid_item(self) -> bool {
		self.material().is_some() && self.shape_family().is_some()
	}

	/// The grid cell `self` occupies, if it is one.
	#[inline]
	pub fn grid_cell(self) -> Option<(Material, ShapeFamily)> {
		Some((self.material()?, self.shape_family()?))
	}

	/// The authored block for a grid cell, or `None` where the catalog has
	/// no such pair. The grid is a GATING rule, not a promise that every
	/// cell exists — nobody is owed snow stairs.
	pub fn from_grid(m: Material, s: ShapeFamily) -> Option<BlockType> {
		BlockType::ALL
			.into_iter()
			.find(|b| b.grid_cell() == Some((m, s)))
	}

	/// How much water this block holds, 1..=8 — None for non-water.
	/// Sources and falling water are full; flowing water carries its
	/// spread level. Pure table lookup: variant identity plays no part.
	#[inline]
	pub fn water_amount(self) -> Option<u8> {
		self.props().fluid.map(|f| f.amount)
	}

	/// Any member of the water family (source, flowing, falling).
	#[inline]
	pub fn is_water(self) -> bool {
		self.props().fluid.is_some()
	}

	#[inline]
	pub fn is_falling(self) -> bool {
		matches!(self.props().fluid, Some(f) if f.falling)
	}

	/// Infinite fluid source, never recomputed by the fluid sim.
	#[inline]
	pub fn is_fluid_source(self) -> bool {
		matches!(self.props().fluid, Some(f) if f.source)
	}

	/// May a placement overwrite a cell holding this block? Air and water
	/// yield; every real block must be dug first.
	///
	/// THE one authority, shared by the client's prediction and the
	/// server's validation — the two must agree exactly or a place either
	/// desyncs or gets rejected after being predicted.
	///
	/// Deliberately not `!props().collides`, which is what the door's upper
	/// half used to ask. An OPEN door has empty collision boxes, so that
	/// test called it free: you could aim at the floor inside a doorway
	/// (the plain ray passes through an open leaf), right-click, and
	/// overwrite the door — no drop, no hold-to-dig timer, and its upper
	/// half left behind as an orphan whose partner no longer resolves.
	#[inline]
	pub fn is_replaceable(self) -> bool {
		matches!(self, BlockType::Air) || self.is_water()
	}

	/// The flowing-water block for an amount 1..=7.
	#[inline]
	pub fn water_flow(amount: u8) -> BlockType {
		debug_assert!((1..=7).contains(&amount));
		Self::from_id(BlockType::WaterFlow1 as u16 + amount as u16 - 1)
	}

	/// Hold-to-dig duration in sim ticks; None = not diggable/collectible.
	#[inline]
	pub fn hardness_ticks(self) -> Option<u16> {
		let h = self.props().hardness_ticks;
		(h > 0).then_some(h)
	}

	/// THE dig-permission gate — hold length for digging this block at
	/// world height `y`, or None when it can't be dug at all. Combines
	/// the per-block hardness with the bedrock rule: the world floor
	/// (y = `BEDROCK_Y`) is indestructible, so the bottom of the world
	/// can never be opened. Client prediction, the server's hold + edit
	/// validation, and the bot all route through this one predicate;
	/// worldgen upholds the same floor by never carving y = 0 (see
	/// worldgen/caves.rs).
	#[inline]
	pub fn dig_hardness(self, y: i32) -> Option<u16> {
		if y <= BEDROCK_Y {
			return None;
		}
		self.hardness_ticks()
	}

	/// What a completed dig credits to the inventory: the `drops`
	/// override, else the block itself. None for undiggable blocks.
	#[inline]
	pub fn dig_drop(self) -> Option<BlockType> {
		self.hardness_ticks()?;
		Some(self.props().drops.unwrap_or(self))
	}

	/// Does holding this type claim a hotbar or inventory slot?
	///
	/// Everything does except water: the only water a player can carry is
	/// the one charge in the water tool, and the tool has its own
	/// permanent slot to show it in. Its count still lives in the
	/// [`crate::inventory::Inventory`] — that is where the charge is
	/// stored, synced and persisted — it simply never becomes an item you
	/// arrange. [`crate::inv_grid::reconcile_layout`] is the enforcer, so
	/// saves written while water WAS an item drop their water slots on
	/// the first reconcile.
	#[inline]
	pub fn is_slottable(self) -> bool {
		self != BlockType::Water && !self.is_tool()
	}

	/// A tool id: an item-only row that lives in the dedicated tool slot,
	/// never in a block slot or the grid, and never in the world.
	#[inline]
	pub fn is_tool(self) -> bool {
		matches!(self, BlockType::Bucket)
	}

	/// Tools every player owns unconditionally (no craft, no count):
	/// the bucket, exactly as it has been since the water tool shipped.
	#[inline]
	pub fn always_owned(self) -> bool {
		self == BlockType::Bucket
	}

	/// A "whole block" for the water-regen rule's floor check: solid
	/// ground or a full water source. Flow/fall water deliberately does
	/// NOT count — otherwise the flow pooling under a pair of floating
	/// sources would let them grow a hanging source shelf.
	#[inline]
	pub fn is_whole(self) -> bool {
		self.props().collides || self.is_fluid_source()
	}

	#[inline]
	pub fn is_stair(self) -> bool {
		matches!(self.props().shape, Some(ShapeKind::Stairs))
	}

	#[inline]
	pub fn is_door(self) -> bool {
		matches!(self.props().shape, Some(ShapeKind::Door))
	}

	#[inline]
	pub fn is_slab(self) -> bool {
		matches!(self.props().shape, Some(ShapeKind::Slab))
	}

	/// The full block a slab stack completes into on its third grow: the
	/// same material in the `Block` shape. Derived rather than tabled, so a
	/// new material's slab cannot be authored without something to complete
	/// into (`every_slab_completes_into_its_material` pins that).
	#[inline]
	pub fn slab_base(self) -> Option<BlockType> {
		if !self.is_slab() {
			return None;
		}
		BlockType::from_grid(self.material()?, ShapeFamily::Block)
	}

	/// Was this block necessarily crafted? Worldgen places none of these,
	/// so every one in a world came off the bench — which is why digging
	/// one asks for the recipe's own Alchemy level
	/// (`crate::unlock::pickup`).
	#[inline]
	pub fn is_crafted_only(self) -> bool {
		self.props().crafted
	}

	/// Can this block be broken down at the bench at all? The full
	/// per-essence yield table lives in [`crate::alchemy::extract_base`]
	/// — this is just its non-empty test, kept here for callers that
	/// only filter.
	#[inline]
	pub fn is_extractable(self) -> bool {
		crate::alchemy::extract_total_base(self) > 0
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn discriminants_index_props() {
		for (i, &b) in BlockType::ALL.iter().enumerate() {
			assert_eq!(b as usize, i, "{b:?} discriminant matches ALL order");
			assert_eq!(BlockType::from_id(b as u16), b, "from_id roundtrip");
			assert_eq!(BlockType::try_from_id(b as u16), Some(b));
		}
		assert_eq!(
			BlockType::from_id(1000),
			BlockType::Air,
			"out of range → Air"
		);
		assert_eq!(BlockType::try_from_id(1000), None, "strict decode refuses");
	}

	#[test]
	fn table_sanity() {
		for &b in &BlockType::ALL {
			let p = b.props();
			// Class-None is Air and the item-only tools, nothing else —
			// and nothing colliding is None.
			assert_eq!(
				p.class == RenderClass::None,
				b == BlockType::Air || b.is_tool()
			);
			if p.collides {
				assert_ne!(p.class, RenderClass::None);
			}
			// Shape tag and render class travel together, and every shaped
			// block collides (physics resolves its sub-boxes/ramp).
			assert_eq!(p.shape.is_some(), p.class == RenderClass::Shaped, "{b:?}");
			if p.shape.is_some() {
				assert!(p.collides, "{b:?} shaped blocks collide");
			}
		}
	}

	#[test]
	fn dig_table_sanity() {
		for &b in &BlockType::ALL {
			let p = b.props();
			// Every solid block can be dug; only the source in the water
			// family can (the water tool's scoop) — flows/falls cannot.
			if p.collides {
				assert!(b.hardness_ticks().is_some(), "{b:?} is diggable");
			}
			if b.is_water() {
				assert_eq!(
					b.hardness_ticks().is_some(),
					b.is_fluid_source(),
					"{b:?} collectible iff source"
				);
			}
			assert!(BlockType::Air.hardness_ticks().is_none());
			// Every diggable block credits something valid and diggable
			// itself (so the credit is placeable and re-collectible).
			match b.dig_drop() {
				Some(d) => {
					assert!(b.hardness_ticks().is_some(), "{b:?}");
					assert!(
						d.hardness_ticks().is_some(),
						"{b:?} drops re-diggable {d:?}"
					);
					assert_ne!(d, BlockType::Air, "{b:?} never drops Air");
				}
				None => assert!(b.hardness_ticks().is_none(), "{b:?}"),
			}
		}
	}

	/// The family assignment is an explicit roster, so a new row's family
	/// is a deliberate choice, never an accident of the builder default.
	#[test]
	fn dig_families_are_pinned() {
		use DigFamily::*;
		let expect = [
			(BlockType::Water, Fixed), // scoop time = the viewmodel swing
			(BlockType::Leaves, Fixed),
			(BlockType::PineLeaves, Fixed),
			(BlockType::Torch, Fixed),
			(BlockType::Dirt, Soft),
			(BlockType::Grass, Soft),
			(BlockType::DryGrass, Soft),
			(BlockType::Sand, Soft),
			(BlockType::Mud, Soft),
			(BlockType::Snow, Soft),
			(BlockType::Wood, Wood),
			(BlockType::WoodStairs, Wood),
			(BlockType::WoodSlab, Wood),
			(BlockType::LumberDoor, Wood),
			(BlockType::Lumber, Wood),
			(BlockType::LumberStairs, Wood),
			(BlockType::LumberSlab, Wood),
			(BlockType::DarkLumber, Wood),
			(BlockType::DarkLumberStairs, Wood),
			(BlockType::DarkLumberSlab, Wood),
			(BlockType::DarkLumberDoor, Wood),
			(BlockType::RedLumber, Wood),
			(BlockType::RedLumberStairs, Wood),
			(BlockType::RedLumberSlab, Wood),
			(BlockType::RedLumberDoor, Wood),
			(BlockType::Table, Wood),
			(BlockType::Chair, Wood),
			(BlockType::Bed, Wood),
			(BlockType::Stone, Rock),
			(BlockType::StoneStairs, Rock),
			(BlockType::StoneSlab, Rock),
			(BlockType::StoneDoor, Rock),
			(BlockType::Sandstone, Rock),
			(BlockType::SandstoneStairs, Rock),
			(BlockType::SandstoneSlab, Rock),
			(BlockType::SandstoneDoor, Rock),
			(BlockType::Gravel, Rock),
			(BlockType::Rubble, Rock),
			(BlockType::Emberore, Rock),
			(BlockType::Duskstone, Rock),
			(BlockType::Glowvein, Rock),
			(BlockType::Aetherock, Rock),
			(BlockType::DepletedStone, Rock),
		];
		for (b, f) in expect {
			assert_eq!(b.dig_family(), f, "{b:?}");
		}
		// The roster covers every diggable block — a new diggable row
		// must be added above.
		let diggable = BlockType::ALL
			.iter()
			.filter(|b| b.hardness_ticks().is_some())
			.count();
		assert_eq!(expect.len(), diggable, "roster covers every diggable block");
	}

	/// The material assignment is an explicit roster for the same reason
	/// the dig families are: it decides a block's unlock level, its craft
	/// cost, its extraction yield and its rock gate all at once, so a new
	/// row picking one up by accident would move four numbers silently.
	/// The blocks WITHOUT a material are the other half of the decision —
	/// functional items, priced by hand (§7.3).
	#[test]
	fn material_roster_is_pinned() {
		use BlockType as B;
		use Material as M;
		let expect = [
			(B::Dirt, M::Dirt),
			(B::Grass, M::Grass),
			(B::DryGrass, M::DryGrass),
			(B::Sand, M::Sand),
			(B::Mud, M::Mud),
			(B::Snow, M::Snow),
			(B::Stone, M::Stone),
			(B::StoneStairs, M::Stone),
			(B::StoneSlab, M::Stone),
			(B::StoneDoor, M::Stone),
			(B::Sandstone, M::Sandstone),
			(B::SandstoneStairs, M::Sandstone),
			(B::SandstoneSlab, M::Sandstone),
			(B::SandstoneDoor, M::Sandstone),
			(B::Gravel, M::Gravel),
			(B::Rubble, M::Rubble),
			(B::Emberore, M::Emberore),
			(B::Duskstone, M::Duskstone),
			(B::Glowvein, M::Glowvein),
			(B::Aetherock, M::Aetherock),
			(B::DepletedStone, M::DepletedStone),
			(B::Wood, M::Wood),
			(B::WoodStairs, M::Wood),
			(B::WoodSlab, M::Wood),
			(B::Lumber, M::Lumber),
			(B::LumberStairs, M::Lumber),
			(B::LumberSlab, M::Lumber),
			(B::LumberDoor, M::Lumber),
			(B::DarkLumber, M::DarkLumber),
			(B::DarkLumberStairs, M::DarkLumber),
			(B::DarkLumberSlab, M::DarkLumber),
			(B::DarkLumberDoor, M::DarkLumber),
			(B::RedLumber, M::RedLumber),
			(B::RedLumberStairs, M::RedLumber),
			(B::RedLumberSlab, M::RedLumber),
			(B::RedLumberDoor, M::RedLumber),
			(B::Leaves, M::Leaf),
			(B::PineLeaves, M::PineLeaf),
		];
		for (b, m) in expect {
			assert_eq!(b.material(), Some(m), "{b:?}");
		}
		for &b in &BlockType::ALL {
			let listed = expect.iter().any(|&(e, _)| e == b);
			assert_eq!(b.material().is_some(), listed, "{b:?}");
		}
		// And the complement, spelled out: a new row joining the
		// material-LESS set is a decision too, because it opts the block
		// out of the grid and into being hand-priced.
		let functional: Vec<BlockType> = BlockType::ALL
			.into_iter()
			.filter(|b| b.material().is_none())
			.collect();
		assert_eq!(
			functional,
			vec![
				B::Air,
				B::Water,
				B::WaterFlow1,
				B::WaterFlow2,
				B::WaterFlow3,
				B::WaterFlow4,
				B::WaterFlow5,
				B::WaterFlow6,
				B::WaterFlow7,
				B::WaterFall,
				B::Torch,
				B::Bucket,
				B::Table,
				B::Chair,
				B::Bed,
			]
		);
	}

	/// Grid membership: a substance in a form. The torch is the standing
	/// counter-example — a shaped block that is a THING, not a form any
	/// material could be worked into, so it has no family and is priced by
	/// hand instead.
	#[test]
	fn the_grid_is_material_times_shape() {
		use BlockType as B;
		assert_eq!(
			B::Stone.grid_cell(),
			Some((Material::Stone, ShapeFamily::Block))
		);
		assert_eq!(
			B::LumberDoor.grid_cell(),
			Some((Material::Lumber, ShapeFamily::Door))
		);
		assert_eq!(B::Torch.shape_family(), None, "a torch is not a form");
		assert!(!B::Torch.is_grid_item());
		assert!(!B::Bucket.is_grid_item());
		assert!(!B::Water.is_grid_item());
		// `from_grid` is the inverse of `grid_cell` wherever a pair is
		// authored, and each cell is authored at most once.
		for &b in &BlockType::ALL {
			if let Some((m, s)) = b.grid_cell() {
				assert_eq!(BlockType::from_grid(m, s), Some(b), "{b:?}");
			}
		}
		for &m in &Material::ALL {
			for &s in &ShapeFamily::ALL {
				let n = BlockType::ALL
					.iter()
					.filter(|b| b.grid_cell() == Some((m, s)))
					.count();
				assert!(n <= 1, "{m:?} × {s:?} authored {n} times");
			}
		}
	}

	/// A slab has to have something to complete into — that is why the
	/// `Lumber` block exists at all. Derived from the material, so a new
	/// timber's slab cannot ship without its cube.
	#[test]
	fn every_slab_completes_into_its_material() {
		for &b in &BlockType::ALL {
			if !b.is_slab() {
				assert_eq!(b.slab_base(), None, "{b:?} is not a slab");
				continue;
			}
			let base = b.slab_base().unwrap_or_else(|| panic!("{b:?} has no base"));
			assert_eq!(base.material(), b.material(), "{b:?}");
			assert_eq!(base.shape_family(), Some(ShapeFamily::Block), "{b:?}");
		}
		use BlockType as B;
		assert_eq!(B::WoodSlab.slab_base(), Some(B::Wood));
		assert_eq!(B::StoneSlab.slab_base(), Some(B::Stone));
		assert_eq!(B::LumberSlab.slab_base(), Some(B::Lumber));
	}

	#[test]
	fn bedrock_floor_is_undiggable() {
		for &b in &BlockType::ALL {
			assert_eq!(b.dig_hardness(BEDROCK_Y), None, "{b:?} undiggable at floor");
			assert_eq!(
				b.dig_hardness(BEDROCK_Y + 1),
				b.hardness_ticks(),
				"{b:?} normal hardness above the floor"
			);
		}
	}

	#[test]
	fn water_family_props() {
		for &b in &BlockType::ALL {
			match b.water_amount() {
				Some(a) => {
					assert!((1..=8).contains(&a), "{b:?} amount {a}");
					assert_eq!(b.props().class, RenderClass::Translucent, "{b:?}");
					assert!(!b.props().collides, "{b:?} never collides");
					assert!(b.is_water());
				}
				None => assert!(!b.is_water()),
			}
		}
		assert_eq!(BlockType::Water.water_amount(), Some(8), "source is full");
		assert_eq!(BlockType::WaterFall.water_amount(), Some(8), "fall is full");
		assert!(BlockType::WaterFall.is_falling());
		assert!(!BlockType::Water.is_falling());
		for a in 1..=7u8 {
			assert_eq!(BlockType::water_flow(a).water_amount(), Some(a));
		}
	}

	/// The table is now the source of truth; this pins it to the old
	/// concrete-variant semantics for every block, so the generalization
	/// cannot have changed behavior.
	#[test]
	fn fluid_table_matches_legacy_variant_semantics() {
		for &b in &BlockType::ALL {
			let legacy_amount = match b {
				BlockType::Water | BlockType::WaterFall => Some(8u8),
				BlockType::WaterFlow1 => Some(1),
				BlockType::WaterFlow2 => Some(2),
				BlockType::WaterFlow3 => Some(3),
				BlockType::WaterFlow4 => Some(4),
				BlockType::WaterFlow5 => Some(5),
				BlockType::WaterFlow6 => Some(6),
				BlockType::WaterFlow7 => Some(7),
				_ => None,
			};
			assert_eq!(b.water_amount(), legacy_amount, "{b:?}");
			assert_eq!(b.is_water(), legacy_amount.is_some(), "{b:?}");
			assert_eq!(b.is_falling(), b == BlockType::WaterFall, "{b:?}");
			assert_eq!(b.is_fluid_source(), b == BlockType::Water, "{b:?}");
			// Today Translucent ⇔ Water bucket; the split exists so a
			// future glass can break the equivalence deliberately.
			assert_eq!(
				b.props().bucket == MeshBucket::Water,
				b.props().class == RenderClass::Translucent,
				"{b:?}"
			);
			assert_eq!(b.props().fluid.is_some(), b.is_water(), "{b:?}");
		}
		for a in 1..=7u8 {
			let f = BlockType::water_flow(a)
				.props()
				.fluid
				.expect("flow is fluid");
			assert_eq!((f.amount, f.source, f.falling), (a, false, false));
		}
	}
}
