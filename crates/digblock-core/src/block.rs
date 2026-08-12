//! Block types and their data-driven properties. Adding a block is: one
//! `blocks!` row, a texture PNG, and a `LAYERS` entry in digblock-assets
//! — nothing else.
//!
//! Values are contiguous so they index tables directly (the Java used
//! AIR=-1, WATER=0, DIRT=1, GRASS=2; shifted by +1 here so a chunk is a
//! flat array of cells with AIR=0 as the natural default).

use crate::dir::Dir;

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
	Dirt = 2 => dig(carveable(props(RenderClass::Opaque, true, tex::DIRT, tex::DIRT, tex::DIRT)), 30, DigFamily::Soft),
	Grass = 3 => dig(carveable(props(RenderClass::Opaque, true, tex::GRASS_SIDE, tex::GRASS_TOP, tex::DIRT)), 30, DigFamily::Soft),
	Sand = 4 => dig(props(RenderClass::Opaque, true, tex::SAND, tex::SAND, tex::SAND), 25, DigFamily::Soft),
	/// Mined stone drops Rubble (SKILLS-DESIGN §7.1): the clean block
	/// exists only pristine in terrain or transmuted at the bench, which
	/// is what makes transmutation the economy rather than a curiosity.
	Stone = 5 => drops(dig(carveable(props(RenderClass::Opaque, true, tex::STONE, tex::STONE, tex::STONE)), 90, DigFamily::Rock), BlockType::Rubble),
	Wood = 6 => dig(props(RenderClass::Opaque, true, tex::WOOD, tex::WOOD_TOP, tex::WOOD_TOP), 60, DigFamily::Wood),
	Leaves = 7 => dig(props(RenderClass::Cutout, true, tex::LEAVES, tex::LEAVES, tex::LEAVES), 10, DigFamily::Fixed),
	Snow = 8 => dig(carveable(props(RenderClass::Opaque, true, tex::SNOW, tex::SNOW, tex::SNOW)), 15, DigFamily::Soft),
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
	DryGrass = 17 => dig(carveable(props(RenderClass::Opaque, true, tex::DRY_GRASS_SIDE, tex::DRY_GRASS_TOP, tex::DIRT)), 30, DigFamily::Soft),
	/// Marsh surface and subsoil: wet dark earth.
	Mud = 18 => dig(carveable(props(RenderClass::Opaque, true, tex::MUD, tex::MUD, tex::MUD)), 25, DigFamily::Soft),
	/// Steep-shore and scree surface: loose pebbled rock.
	Gravel = 19 => dig(carveable(props(RenderClass::Opaque, true, tex::GRAVEL, tex::GRAVEL, tex::GRAVEL)), 45, DigFamily::Rock),
	/// Conifer foliage; a structure block like Leaves, so not carveable.
	PineLeaves = 20 => dig(props(RenderClass::Cutout, true, tex::PINE_LEAVES, tex::PINE_LEAVES, tex::PINE_LEAVES), 10, DigFamily::Fixed),
	/// The stratum under sandy floors (ocean, beach, river, lake beds).
	/// "All broken rock": mined sandstone is Rubble too, so the clean
	/// block is craft-only like Stone's.
	Sandstone = 21 => drops(dig(carveable(props(RenderClass::Opaque, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), 70, DigFamily::Rock), BlockType::Rubble),
	/// 3-step stairs (12ths sub-grid; facing in the cell state nibble).
	/// Crafted via alchemy from sub-wood, never generated.
	WoodStairs = 22 => dig(shaped(props(RenderClass::Shaped, true, tex::WOOD, tex::WOOD_TOP, tex::WOOD_TOP), ShapeKind::Stairs), 60, DigFamily::Wood),
	StoneStairs = 23 => dig(shaped(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE), ShapeKind::Stairs), 90, DigFamily::Rock),
	/// Two-cell door (facing/open/half in the state nibble): thin leaf,
	/// divided-lite window in the upper half. Crafted via alchemy.
	WoodDoor = 24 => dig(shaped(props(RenderClass::Shaped, true, tex::DOOR_WOOD, tex::WOOD_TOP, tex::WOOD_TOP), ShapeKind::Door), 45, DigFamily::Wood),
	/// Stackable bottom-anchored third-slab (state bit 0 = two-thirds).
	/// Placing onto a slab's top face grows it; the third grow converts
	/// the cell to the plain base block. Hardness is a third of the base
	/// block's, since each dig removes one third. Crafted via alchemy.
	WoodSlab = 25 => dig(shaped(props(RenderClass::Shaped, true, tex::WOOD, tex::WOOD_TOP, tex::WOOD_TOP), ShapeKind::Slab), 20, DigFamily::Wood),
	StoneSlab = 26 => dig(shaped(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE), ShapeKind::Slab), 30, DigFamily::Rock),
	/// What mined rock actually is (SKILLS-DESIGN §7.1): cracked, fully
	/// placeable, and the mass Earth feedstock at the bench. Placed, it
	/// reads as rough cobble — the free rustic building material. Drops
	/// itself, so it round-trips.
	Rubble = 27 => dig(props(RenderClass::Opaque, true, tex::RUBBLE, tex::RUBBLE, tex::RUBBLE), 90, DigFamily::Rock),
	/// First light (SKILLS-DESIGN §7.3/§7.4): a floor-mounted post with a
	/// glowing head, crafted at Alch 8 — before ore on purpose. Stage-A
	/// emissive (fullbright ink) AND a Stage-B source: it seeds level 14
	/// into the voxel light field (LIGHT-DESIGN.md).
	Torch = 28 => dig(emits(glows(shaped(props(RenderClass::Shaped, true, tex::TORCH_SIDE, tex::TORCH_FLAME, tex::TORCH_SIDE), ShapeKind::Torch)), 14), 10, DigFamily::Fixed),
	/// The water tool as an ITEM-ONLY row (SKILLS-DESIGN §6): tools are
	/// blocks that never render, collide, place, or slot — they exist so
	/// ownership, prediction, rebase, and both persistence formats ride
	/// the count machinery untouched. The bucket is owned by everyone
	/// always ([`BlockType::always_owned`]); its charge stays the Water
	/// count. Texture layers are 0 (never sampled — RenderClass::None).
	Bucket = 29 => props(RenderClass::None, false, 0, 0, 0),
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
	/// besides (`skills::dig_level_gate`).
	Rock,
}

const fn props(class: RenderClass, collides: bool, side: u16, top: u16, bottom: u16) -> BlockProps {
	BlockProps {
		class,
		collides,
		bucket: MeshBucket::Solid,
		fluid: None,
		carveable: false,
		tex_side: side,
		tex_top: top,
		tex_bottom: bottom,
		hardness_ticks: 0,
		drops: None,
		shape: None,
		emissive: false,
		light: 0,
		dig_family: DigFamily::Fixed,
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
		tex_side: tex::WATER,
		tex_top: tex::WATER,
		tex_bottom: tex::WATER,
		hardness_ticks: 0,
		drops: None,
		shape: None,
		emissive: false,
		light: 0,
		dig_family: DigFamily::Fixed,
	}
}

/// Texture-array layers, matching digblock_assets::tex::LAYERS order.
mod tex {
	pub const WATER: u16 = 0;
	pub const DIRT: u16 = 1;
	pub const GRASS_SIDE: u16 = 2;
	pub const GRASS_TOP: u16 = 3;
	pub const SAND: u16 = 4;
	pub const STONE: u16 = 5;
	pub const WOOD: u16 = 6;
	pub const WOOD_TOP: u16 = 7;
	pub const LEAVES: u16 = 8;
	pub const SNOW: u16 = 9;
	pub const DRY_GRASS_SIDE: u16 = 10;
	pub const DRY_GRASS_TOP: u16 = 11;
	pub const MUD: u16 = 12;
	pub const GRAVEL: u16 = 13;
	pub const PINE_LEAVES: u16 = 14;
	pub const SANDSTONE: u16 = 15;
	pub const DOOR_WOOD: u16 = 16;
	pub const RUBBLE: u16 = 17;
	pub const TORCH_SIDE: u16 = 18;
	pub const TORCH_FLAME: u16 = 19;
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

	/// The full block a slab stack completes into on its third grow.
	#[inline]
	pub fn slab_base(self) -> Option<BlockType> {
		match self {
			BlockType::WoodSlab => Some(BlockType::Wood),
			BlockType::StoneSlab => Some(BlockType::Stone),
			_ => None,
		}
	}

	/// Can this block be broken down at the bench at all? The full
	/// per-essence yield table lives in [`crate::alchemy::extract_base`]
	/// — this is just its non-empty test, kept here for callers that
	/// only filter.
	#[inline]
	pub fn is_extractable(self) -> bool {
		!crate::alchemy::extract_base(self).is_empty()
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
			(BlockType::WoodDoor, Wood),
			(BlockType::Stone, Rock),
			(BlockType::StoneStairs, Rock),
			(BlockType::StoneSlab, Rock),
			(BlockType::Sandstone, Rock),
			(BlockType::Gravel, Rock),
			(BlockType::Rubble, Rock),
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
