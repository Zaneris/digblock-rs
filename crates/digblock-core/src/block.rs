//! Block types and their data-driven properties. Adding a block is: one
//! `blocks!` row, a texture PNG, and a `LAYERS` entry in digblock-assets
//! — nothing else.
//!
//! Values are contiguous so they index tables directly (the Java numbered
//! air below zero; everything is shifted here so a chunk is a flat array
//! of cells with Air as the natural zero default).

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
	/// A water source: infinite, at full amount. All generated water (oceans,
	/// river pools) is sources — flowing/falling states describe water in
	/// transit under the Minecraft-style flow rules. Diggable only via the
	/// water tool (the fluid-aware ray); the hardness is the scoop time.
	Water = 1 => dig(water_props(8, true, false), 20, DigFamily::Fixed),
	Dirt = 2 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::DIRT, tex::DIRT, tex::DIRT)), 30, DigFamily::Soft), Material::Dirt),
	Grass = 3 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::GRASS_SIDE, tex::GRASS_TOP, tex::DIRT)), 30, DigFamily::Soft), Material::Grass),
	Sand = 4 => mat(dig(props(RenderClass::Opaque, true, tex::SAND, tex::SAND, tex::SAND), 25, DigFamily::Soft), Material::Sand),
	/// Mined stone crumbles to Rubble (SKILLS-DESIGN §7.1): the clean block
	/// exists only pristine in terrain or transmuted at the bench, which
	/// is what makes transmutation the economy rather than a curiosity.
	/// `crumbles` and not `drops`, so a stone somebody MADE comes back
	/// whole — the mountain's own is the only one that breaks.
	Stone = 5 => mat(crumbles(dig(carveable(props(RenderClass::Opaque, true, tex::STONE, tex::STONE, tex::STONE)), 90, DigFamily::Rock), BlockType::Rubble), Material::Stone),
	Wood = 6 => mat(dig(props(RenderClass::Opaque, true, tex::LOG, tex::LOG_TOP, tex::LOG_TOP), 60, DigFamily::Wood), Material::Wood),
	Leaves = 7 => mat(dig(props(RenderClass::Cutout, true, tex::LEAVES, tex::LEAVES, tex::LEAVES), 10, DigFamily::Fixed), Material::Leaf),
	Snow = 8 => mat(dig(carveable(props(RenderClass::Opaque, true, tex::SNOW, tex::SNOW, tex::SNOW)), 15, DigFamily::Soft), Material::Snow),
	/// Flowing water, from the thinnest up to just under a source. Spread
	/// sideways from a source or a landed fall, thinning as it goes
	/// (`fluid::falloff`).
	WaterFlow1 = 9 => water_props(1, false, false),
	WaterFlow2 = 10 => water_props(2, false, false),
	WaterFlow3 = 11 => water_props(3, false, false),
	WaterFlow4 = 12 => water_props(4, false, false),
	WaterFlow5 = 13 => water_props(5, false, false),
	WaterFlow6 = 14 => water_props(6, false, false),
	WaterFlow7 = 15 => water_props(7, false, false),
	/// Water falling through air: renders full-height, lands at full amount.
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
	/// "All broken rock": mined sandstone crumbles to Rubble too, so clean
	/// sandstone is bench work exactly as clean stone is, and for the same
	/// reason it may stay craftable.
	Sandstone = 21 => mat(crumbles(dig(carveable(props(RenderClass::Opaque, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), 70, DigFamily::Rock), BlockType::Rubble), Material::Sandstone),
	/// Stepped stairs on the sub-grid (geometry in `shapes.rs`; facing in
	/// the cell state nibble). Crafted via alchemy from sub-wood, never
	/// generated.
	WoodStairs = 22 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LOG, tex::LOG_TOP, tex::LOG_TOP)), ShapeKind::Stairs), 60, DigFamily::Wood), Material::Wood),
	StoneStairs = 23 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE)), ShapeKind::Stairs), 90, DigFamily::Rock), Material::Stone),
	/// Two-cell door (facing/open/half in the state nibble): thin leaf,
	/// divided-lite window in the upper half. Crafted via alchemy — out
	/// of planed `Lumber` like the rest of the joinery, which is why it
	/// wears LUMBER_LIGHT and unlocks with the other lumber shapes.
	LumberDoor = 24 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Door), 45, DigFamily::Wood), Material::Lumber),
	/// Stackable bottom-anchored third-slab (its thickness and anchor live
	/// in the state nibble; `shapes::slab_state`). Placing onto a slab's
	/// top face grows it; the third grow converts the cell to the plain
	/// base block. Hardness is a third of the base block's, since each dig
	/// removes one third. Crafted via alchemy.
	WoodSlab = 25 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LOG, tex::LOG_TOP, tex::LOG_TOP)), ShapeKind::Slab), 20, DigFamily::Wood), Material::Wood),
	StoneSlab = 26 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE)), ShapeKind::Slab), 30, DigFamily::Rock), Material::Stone),
	/// What mined rock actually is (SKILLS-DESIGN §7.1): cracked, fully
	/// placeable, and the mass Terra feedstock at the bench. Placed, it
	/// reads as rough cobble — the free rustic building material. Drops
	/// itself, so it round-trips.
	Rubble = 27 => mat(dig(props(RenderClass::Opaque, true, tex::RUBBLE, tex::RUBBLE, tex::RUBBLE), 90, DigFamily::Rock), Material::Rubble),
	/// First light (SKILLS-DESIGN §7.3/§7.4): a floor-mounted post with a
	/// glowing head, crafted early — before ore on purpose. It both
	/// `glows` (fullbright ink, so the head reads bright of itself) and
	/// `emits`, which is what puts it in the GI light list.
	Torch = 28 => dig(emits(glows(shaped(crafted(props(RenderClass::Shaped, true, tex::TORCH_SIDE, tex::TORCH_FLAME, tex::TORCH_SIDE)), ShapeKind::Torch)), 14), 10, DigFamily::Fixed),
	/// The water tool as an ITEM-ONLY row (SKILLS-DESIGN §6): tools are
	/// blocks that never render, collide, place, or slot — they exist so
	/// ownership, prediction, rebase, and both persistence formats ride
	/// the count machinery untouched. The bucket is the catalog's FIRST
	/// tool (SKILLS-DESIGN §13.5): crafted like the other three, priced
	/// so the opening dig-and-extract loop pays for it, and granted by
	/// migration to every record written while it was still owned by
	/// everyone. Its charge stays the Water count. Its texture layers are
	/// placeholders, never sampled — RenderClass::None.
	Bucket = 29 => props(RenderClass::None, false, 0, 0, 0),
	/// Planed timber (SKILLS-DESIGN §7.2's material × shape grid): the
	/// refined counterpart of the felled `Wood` trunk, craft-only and
	/// never generated — the bench's second timber, opened after the log's
	/// own shapes. Exists as a full block because its slab has to
	/// have something to complete into (three thirds = one block, the
	/// rule that closes the slab economy).
	Lumber = 30 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), 60, DigFamily::Wood), Material::Lumber),
	LumberStairs = 31 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Stairs), 60, DigFamily::Wood), Material::Lumber),
	LumberSlab = 32 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT, tex::LUMBER_LIGHT)), ShapeKind::Slab), 20, DigFamily::Wood), Material::Lumber),
	/// The mason's door — §7.2's grid at work rather than a curated row: it
	/// lands where clean Stone does, because that is the material's level,
	/// and it arrives WITH the stone block, slab and stairs because those
	/// shapes were learned long before. Hardness keeps the timber door's
	/// ratio against its own base block.
	StoneDoor = 33 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE, tex::STONE, tex::STONE)), ShapeKind::Door), 68, DigFamily::Rock), Material::Stone),
	/// Sandstone's set, likewise free: the material opens at its own level
	/// and hands over every shape at once.
	SandstoneStairs = 34 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), ShapeKind::Stairs), 70, DigFamily::Rock), Material::Sandstone),
	SandstoneSlab = 35 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), ShapeKind::Slab), 23, DigFamily::Rock), Material::Sandstone),
	SandstoneDoor = 36 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE, tex::SANDSTONE, tex::SANDSTONE)), ShapeKind::Door), 53, DigFamily::Rock), Material::Sandstone),
	/// The second timber. `LUMBER_DARK` was drawn and registered
	/// with `LUMBER_LIGHT` and then sat unused on no block for a release —
	/// under the grid a whole new joinery set is four rows and no new art.
	DarkLumber = 37 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), 60, DigFamily::Wood), Material::DarkLumber),
	DarkLumberStairs = 38 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), ShapeKind::Stairs), 60, DigFamily::Wood), Material::DarkLumber),
	DarkLumberSlab = 39 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), ShapeKind::Slab), 20, DigFamily::Wood), Material::DarkLumber),
	DarkLumberDoor = 40 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::LUMBER_DARK, tex::LUMBER_DARK, tex::LUMBER_DARK)), ShapeKind::Door), 45, DigFamily::Wood), Material::DarkLumber),
	/// The third timber.
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
	/// The four ores (SKILLS-DESIGN §5.3). These rows exist for the reveal
	/// stamp and for nothing else you can hold: worldgen places none of
	/// them, because ore is DERIVED — `build_column` still returns Stone
	/// and the journal still carries Stone, which is what lets veins appear
	/// in worlds that already exist. Not `crafted` either: the bench cannot
	/// make ore, so gating its pickup would gate a thing with no recipe to
	/// gate it by.
	///
	/// **A vein is world geometry, never an item.** Siphoning it draws the
	/// essence straight into your stocks and leaves the tier's own spent
	/// rock standing (`ore::spent_rock`), and THAT is what you break and
	/// carry home. So `is_slottable` is false for all four and nothing can
	/// credit one — which is also why the question "does a placed ore block
	/// glow?" has no answer to get wrong.
	///
	/// All four `seams`: a revealed vein lights the room it opens into,
	/// from the faces that open into it and no others. The two deep tiers
	/// wore the ink already; the shallow pair join them here, which is what
	/// their darkened texture bodies are for (GI-DESIGN.md R31).
	Emberore = 48 => mat(glows(seams(dig(props(RenderClass::Opaque, true, tex::EMBERORE, tex::EMBERORE, tex::EMBERORE), 120, DigFamily::Rock), 2)), Material::Emberore),
	Duskstone = 49 => mat(glows(seams(dig(props(RenderClass::Opaque, true, tex::DUSKSTONE, tex::DUSKSTONE, tex::DUSKSTONE), 150, DigFamily::Rock), 4)), Material::Duskstone),
	Glowvein = 50 => mat(glows(seams(dig(props(RenderClass::Opaque, true, tex::GLOWVEIN, tex::GLOWVEIN, tex::GLOWVEIN), 190, DigFamily::Rock), 4)), Material::Glowvein),
	Aetherock = 51 => mat(glows(seams(dig(props(RenderClass::Opaque, true, tex::AETHEROCK, tex::AETHEROCK, tex::AETHEROCK), 240, DigFamily::Rock), 6)), Material::Aetherock),
	/// The generic grey tapped face the per-tier rocks at the end of this
	/// table replaced. Kept, and unreachable: nothing writes it any more,
	/// but journals and saves made before those rocks existed carry cells
	/// that hold it, and an id that stops decoding is a world that stops
	/// loading. It still breaks into Rubble, which is what it always did.
	///
	/// `drops` and NOT `crumbles`, unlike the two live rocks: every cell
	/// that names this was written by an old siphon draw, so every one of
	/// them is edited and the conditional form would hand out a block that
	/// has never been obtainable.
	DepletedStone = 52 => mat(drops(dig(props(RenderClass::Opaque, true, tex::DEPLETED_STONE, tex::DEPLETED_STONE, tex::DEPLETED_STONE), 90, DigFamily::Rock), BlockType::Rubble), Material::DepletedStone),
	/// The Essence Siphon (SKILLS-DESIGN §5.3/§7.3): an item-only tool row
	/// like the bucket, but CRAFTED rather than owned from the start.
	/// Held, a dig-hold on a revealed vein draws the ore out
	/// and leaves DepletedStone; without it a revealed vein refuses the
	/// hold entirely, so nobody stone-digs a seam away by accident.
	///
	/// Its price carries no Ferrum, ever: ore needs the siphon and Ferrum
	/// needs ore, so a Ferrum-priced siphon deadlocks the economy on the
	/// first world where nobody has one.
	///
	/// It CASTS, and only just: the barrel holds drawn essence and the
	/// work it exists for happens at the bottom of a mine, but it is an
	/// extraction tool that happens to glow rather than a lamp. A green
	/// pool at the feet, and the Resonant Pick well above it — the tool
	/// you MINE with is the one worth carrying for its light.
	/// `emissive` is
	/// deliberately NOT set — a tool is not fullbright, only the one face
	/// `item_geom` marks as its glow is (the torch is one material and
	/// can afford a per-block flag; a tool has a body worth shading).
	/// The tint comes from the texture, which is the whole reason a row
	/// that renders nothing carries one.
	EssenceSiphon = 53 => emits(props(RenderClass::None, false, tex::SIPHON_GLOW, tex::SIPHON_GLOW, tex::SIPHON_GLOW), 5),
	/// The Resonant Pick (SKILLS-DESIGN §5.6): crafted and wielded at the
	/// same level in either skill — the flagship cross-skill dependency,
	/// and the game's biggest essence sink. Held, one hold breaks the plane
	/// of the face you hit, over a square that widens again later in Mining.
	///
	/// This is the level it casts at in ANY hand. It is the one light in
	/// the game that answers to its wielder — see
	/// [`crate::gi::carried_light`], which raises it at the rank that
	/// widens the swing, so the tool's two ranks are one progression the
	/// player can see.
	ResonantPick = 54 => emits(props(RenderClass::None, false, tex::PICK_GLOW, tex::PICK_GLOW, tex::PICK_GLOW), 9),
	/// §7.2's dressed stones — the same rock, worked. Free under the
	/// grid: a material row, a texture, and every shape it is authored
	/// in unlocks with it.
	StoneBrick = 55 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::STONE_BRICK, tex::STONE_BRICK, tex::STONE_BRICK)), 90, DigFamily::Rock), Material::StoneBrick),
	StoneBrickStairs = 56 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE_BRICK, tex::STONE_BRICK, tex::STONE_BRICK)), ShapeKind::Stairs), 90, DigFamily::Rock), Material::StoneBrick),
	StoneBrickSlab = 57 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE_BRICK, tex::STONE_BRICK, tex::STONE_BRICK)), ShapeKind::Slab), 30, DigFamily::Rock), Material::StoneBrick),
	StoneBrickDoor = 58 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::STONE_BRICK, tex::STONE_BRICK, tex::STONE_BRICK)), ShapeKind::Door), 68, DigFamily::Rock), Material::StoneBrick),
	PolishedStone = 59 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::POLISHED_STONE, tex::POLISHED_STONE, tex::POLISHED_STONE)), 90, DigFamily::Rock), Material::PolishedStone),
	PolishedStoneStairs = 60 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::POLISHED_STONE, tex::POLISHED_STONE, tex::POLISHED_STONE)), ShapeKind::Stairs), 90, DigFamily::Rock), Material::PolishedStone),
	PolishedStoneSlab = 61 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::POLISHED_STONE, tex::POLISHED_STONE, tex::POLISHED_STONE)), ShapeKind::Slab), 30, DigFamily::Rock), Material::PolishedStone),
	SandstoneBrick = 62 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::SANDSTONE_BRICK, tex::SANDSTONE_BRICK, tex::SANDSTONE_BRICK)), 70, DigFamily::Rock), Material::SandstoneBrick),
	SandstoneBrickStairs = 63 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE_BRICK, tex::SANDSTONE_BRICK, tex::SANDSTONE_BRICK)), ShapeKind::Stairs), 70, DigFamily::Rock), Material::SandstoneBrick),
	SandstoneBrickSlab = 64 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::SANDSTONE_BRICK, tex::SANDSTONE_BRICK, tex::SANDSTONE_BRICK)), ShapeKind::Slab), 23, DigFamily::Rock), Material::SandstoneBrick),
	/// Leaded glass. OPAQUE, and that is the honest half of §12's open
	/// question: see-through glass is a RENDER problem (the depth
	/// prepass writes the whole quad, so a cutout discard would need
	/// the prepass to sample the atlas too), not a catalog one. This is
	/// a building material that reads as glass without pretending the
	/// light goes through it, and it upgrades in place when the render
	/// answer lands.
	Glass = 65 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::GLASS, tex::GLASS, tex::GLASS)), 25, DigFamily::Soft), Material::Glass),
	/// The prestige material: Metal comes only from ore, so a metal
	/// wall is a statement about how long somebody has been mining.
	Metal = 66 => mat(dig(crafted(props(RenderClass::Opaque, true, tex::METAL, tex::METAL, tex::METAL)), 150, DigFamily::Rock), Material::Metal),
	MetalStairs = 67 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::METAL, tex::METAL, tex::METAL)), ShapeKind::Stairs), 150, DigFamily::Rock), Material::Metal),
	MetalSlab = 68 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::METAL, tex::METAL, tex::METAL)), ShapeKind::Slab), 50, DigFamily::Rock), Material::Metal),
	MetalDoor = 69 => mat(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::METAL, tex::METAL, tex::METAL)), ShapeKind::Door), 113, DigFamily::Rock), Material::Metal),
	/// Grown from Aether, and it GLOWS — Stage-A ink only, like the
	/// deep ore seams, so a crystal wall lights nothing and reads in
	/// the dark.
	///
	/// The slab glows too, and that is not a courtesy: light is a property
	/// of the SUBSTANCE, so a piece sawn off a glowing cube glows. The slab
	/// shipped without the bit and the tell was the grow path — three
	/// thirds complete into `Crystal` (`slab_grown` → `slab_base`), so the
	/// identical volume of the identical stuff snapped to fullbright in one
	/// frame. `every_shape_of_a_material_agrees_on_light` is the pin.
	Crystal = 70 => mat(glows(dig(crafted(props(RenderClass::Opaque, true, tex::CRYSTAL, tex::CRYSTAL, tex::CRYSTAL)), 120, DigFamily::Rock)), Material::Crystal),
	CrystalSlab = 71 => mat(glows(dig(shaped(crafted(props(RenderClass::Shaped, true, tex::CRYSTAL, tex::CRYSTAL, tex::CRYSTAL)), ShapeKind::Slab), 40, DigFamily::Rock)), Material::Crystal),
	/// §7.3's lamps: the first Ferrum sink outside tools, and the
	/// standing Aether one. Both are FUNCTIONAL — a lamp is a thing, not
	/// a form — so both are hand-priced, and both cast real light as well
	/// as wearing the ink.
	Lantern = 72 => dig(emits(glows(crafted(props(RenderClass::Opaque, true, tex::LANTERN, tex::LANTERN, tex::LANTERN))), 13), 30, DigFamily::Rock),
	Aetherlamp = 73 => dig(emits(glows(crafted(props(RenderClass::Opaque, true, tex::AETHERLAMP, tex::AETHERLAMP, tex::AETHERLAMP))), 15), 30, DigFamily::Rock),
	/// The Master's Sigil: the catalog's last unlock, priced in Aether
	/// alone, for a thing that does nothing at all. An item-only row like
	/// the tools — never placed, never rendered in the world — because
	/// what it is FOR is being the last line of the catalog.
	MastersSigil = 74 => props(RenderClass::None, false, 0, 0, 0),
	/// The tapped-out vein faces, one per tier: what a field extraction
	/// leaves behind instead of a hole, so mining ore never excavates and
	/// the mountain keeps its shape.
	///
	/// Still the rock it was — same body, same flecks, the light gone out
	/// of them — because a spent seam should read as the vein you worked
	/// rather than as one more grey stone, and because the block you carry
	/// home from it is a trophy of THAT tier. Each drops ITSELF, so the
	/// face you tapped is the thing you take; ordinary chunk data with no
	/// reveal gate, so everyone sees it on your wall whatever they have
	/// levelled into. Neither `glows` nor `seams`: what a siphon takes out
	/// is exactly the light.
	DepletedEmberore = 75 => mat(dig(props(RenderClass::Opaque, true, tex::DEPLETED_EMBERORE, tex::DEPLETED_EMBERORE, tex::DEPLETED_EMBERORE), 90, DigFamily::Rock), Material::DepletedEmberore),
	DepletedDuskstone = 76 => mat(dig(props(RenderClass::Opaque, true, tex::DEPLETED_DUSKSTONE, tex::DEPLETED_DUSKSTONE, tex::DEPLETED_DUSKSTONE), 90, DigFamily::Rock), Material::DepletedDuskstone),
	DepletedGlowvein = 77 => mat(dig(props(RenderClass::Opaque, true, tex::DEPLETED_GLOWVEIN, tex::DEPLETED_GLOWVEIN, tex::DEPLETED_GLOWVEIN), 90, DigFamily::Rock), Material::DepletedGlowvein),
	DepletedAetherock = 78 => mat(dig(props(RenderClass::Opaque, true, tex::DEPLETED_AETHEROCK, tex::DEPLETED_AETHEROCK, tex::DEPLETED_AETHEROCK), 90, DigFamily::Rock), Material::DepletedAetherock),
	/// The three SURFACE nodes (the essence redesign: essence is found,
	/// not ground). Exactly the ore rows' standing, in a different host:
	/// worldgen still places Wood, Stone and Sand — a node is DERIVED
	/// (`worldgen::nodes`), revealed by the mesh stamp at an ALCHEMY
	/// level, drawn in place with the siphon, and anti-duped by the same
	/// edited-bit the ore reveal reads. So nodes appear retroactively in
	/// worlds that already exist, and no journal or golden moves.
	///
	/// **A node is world geometry, never an item** — `is_slottable` is
	/// false for all three, a draw credits essence, and what you carry
	/// home is the spent face below, which drops itself: the trophy rule.
	///
	/// Heartwood is the living core of a worldgen tree — the first node a
	/// fresh player can see, since the whole essence economy starts here.
	Heartwood = 79 => mat(glows(seams(dig(props(RenderClass::Opaque, true, tex::HEARTWOOD, tex::HEARTWOOD, tex::HEARTWOOD), 80, DigFamily::Wood), 2)), Material::Heartwood),
	DrainedHeartwood = 80 => mat(dig(props(RenderClass::Opaque, true, tex::DRAINED_HEARTWOOD, tex::DRAINED_HEARTWOOD, tex::DRAINED_HEARTWOOD), 60, DigFamily::Wood), Material::DrainedHeartwood),
	/// Leystone rides the ore deposit stream as its SURFACE tier — the
	/// shallow band the deep tiers vacate — and manifests in pristine
	/// Stone exactly as they do; only its reveal keys on Alchemy.
	Leystone = 81 => mat(glows(seams(dig(props(RenderClass::Opaque, true, tex::LEYSTONE, tex::LEYSTONE, tex::LEYSTONE), 110, DigFamily::Rock), 3)), Material::Leystone),
	SpentLeystone = 82 => mat(dig(props(RenderClass::Opaque, true, tex::SPENT_LEYSTONE, tex::SPENT_LEYSTONE, tex::SPENT_LEYSTONE), 90, DigFamily::Rock), Material::SpentLeystone),
	/// Springstone manifests in the pristine sand bed of a lake or river
	/// — the one node you dig through water to reach, which is what makes
	/// water features destinations.
	Springstone = 83 => mat(glows(seams(dig(props(RenderClass::Opaque, true, tex::SPRINGSTONE, tex::SPRINGSTONE, tex::SPRINGSTONE), 60, DigFamily::Soft), 3)), Material::Springstone),
	SpentSpringstone = 84 => mat(dig(props(RenderClass::Opaque, true, tex::SPENT_SPRINGSTONE, tex::SPENT_SPRINGSTONE, tex::SPENT_SPRINGSTONE), 25, DigFamily::Soft), Material::SpentSpringstone),
}

/// The indestructible world floor: cells at this height can never be
/// dug (`BlockType::dig_hardness`), and worldgen's cave pass never
/// carves them — so no path exists to a hole in the bottom of the world.
///
/// This is THE number. [`crate::protect::WORLD_FLOOR`] — the write-side
/// half of the same rule — is an alias of it rather than a second
/// literal, so the plane a dig refuses and the plane a write refuses
/// cannot drift apart.
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
	/// Sub-block geometry from the `shapes` registry (sub-grid). Solid
	/// stream; occlusion is per-face via the shape's coverage masks, and
	/// shaped blocks never cast AO (like Cutout).
	Shaped,
}

/// Which shape family a `RenderClass::Shaped` block renders/collides as.
/// The geometry itself (boxes, quads, masks) lives in `crate::shapes` —
/// this stays a plain tag so block.rs keeps sitting below the mesher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeKind {
	/// An even stepped ascent; the state nibble carries the facing.
	Stairs,
	/// Thin two-cell leaf; the state nibble carries facing, open, and
	/// which half of the leaf this cell is.
	Door,
	/// Bottom-anchored partial fill; the state nibble carries the
	/// thickness and the anchor.
	Slab,
	/// Floor-mounted torch: post + glowing head, one stateless variant.
	Torch,
	/// Full-footprint top on four legs; stateless, so tables pushed
	/// together read as one surface.
	Table,
	/// Seat, back and four legs; the state nibble carries the facing (the
	/// back).
	Chair,
	/// A one-cell cot: mattress on a low frame, pillow at the head end;
	/// the state nibble carries the facing (the head).
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

	/// The form's volume in sub-units (`shapes::SUBS_PER_BLOCK` to a whole
	/// cube) — §4.2's volume rule, and the factor every craft cost and
	/// extraction yield in this material is prorated by. Pinned against
	/// the rasterized geometry by
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

/// The five reagents — what a siphon draws a NODE into (SKILLS-DESIGN
/// §14). `repr` indices are a wire and persistence format (flagged
/// inventory pairs `SUB_WIRE_FLAG | index`), APPEND-ONLY.
///
/// **The indices are frozen; the names are not, and were not.** The
/// redesign gave these their reagent identities and the source kept
/// saying Flora/Earth/Aqua/Metal for a release, so the code and every
/// screen spoke different words for one currency. Renaming the variants
/// moves no byte anywhere: the discriminants below are explicit, nothing
/// serializes an `Essence` by name (no serde, no `FromStr`), and the one
/// place a name is parsed — `/give` — reads [`Essence::name`]. The
/// one-time balance reset is NOT what made this safe, and it could not
/// have: Aether survives it and must still decode at its own index, and
/// these ride a live client/server wire that no reset touches.
///
/// The first two indices deliberately keep the retired `Material`
/// enum's numbering — Vitae sits where sub-wood (0) lived and Terra
/// where sub-stone (1) did — so every record and save written under the
/// old two-family economy decodes 1:1 into the right reagent with NO
/// migration code anywhere. The design doc's decode-time migration is
/// this numbering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Essence {
	/// Living matter, drawn from the heartwood in a trunk. Legacy
	/// sub-wood's slot.
	Vitae = 0,
	/// Mineral bulk, drawn from leystone in pristine deep stone. Legacy
	/// sub-stone's slot.
	Terra = 1,
	/// Water, drawn from springstone in a fresh bed — under the surface
	/// of a lake or a river, which is what makes reaching it the trip.
	Unda = 2,
	/// Metal, drawn from the ore tiers.
	Ferrum = 3,
	/// The rare one: cannot be transmuted into, ever, and the only
	/// balance the redesign's one-time reset lets a player keep.
	Aether = 4,
}

impl Essence {
	pub const COUNT: usize = 5;
	pub const ALL: [Essence; Self::COUNT] = [
		Essence::Vitae,
		Essence::Terra,
		Essence::Unda,
		Essence::Ferrum,
		Essence::Aether,
	];

	/// Strict decode of a wire family index.
	#[inline]
	pub fn try_from_index(v: u16) -> Option<Self> {
		Self::ALL.get(v as usize).copied()
	}

	/// The screens' all-caps label — and, via `/give`'s parser, the name
	/// an operator types, so the command and the UI cannot drift.
	///
	/// Still a function rather than a `Debug` of the variant, now that the
	/// two agree: the label is the ALL-CAPS form, and letting a screen
	/// derive it from the identifier would make the identifier a display
	/// format — which is the coupling that stopped these being renamed for
	/// a release in the first place.
	pub fn name(self) -> &'static str {
		match self {
			Essence::Vitae => "VITAE",
			Essence::Terra => "TERRA",
			Essence::Unda => "UNDA",
			Essence::Ferrum => "FERRUM",
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
	/// The water level this block holds, thinnest up (sources and falls are
	/// full).
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
	/// Worldgen never PLACES one of these, so every one standing in a world
	/// was crafted by somebody. That is what lets `unlock::pickup` refuse
	/// the DIG below the block's own `unlock::craft` level — an item you
	/// cannot make yet is an item you cannot mine your way around either.
	/// Pinned against worldgen itself by
	/// `worldgen_never_places_a_crafted_block`.
	///
	/// Not the same question as [`BlockType::is_bench_only`], which asks
	/// whether the world ever HANDS you one, and which this implies without
	/// being implied by. Clean rock is the gap: the world is made of stone
	/// and every dig on it yields Rubble, so the bench is its only source
	/// and it is still not `crafted` — a natural quarry would otherwise
	/// stand behind the craft gate.
	pub crafted: bool,
	/// Texture-array layer indices (order fixed by
	/// digblock_assets::tex::LAYERS). u16 so the full MAX_TEX_LAYERS
	/// range is reachable — the mesher's vertex layer field is what caps it.
	pub tex_side: u16,
	pub tex_top: u16,
	pub tex_bottom: u16,
	/// Hold-to-dig duration in sim ticks; 0 = cannot be dug or
	/// collected. Shared client/server behavior — retuning is a
	/// SIM_VERSION bump.
	pub hardness_ticks: u16,
	/// Inventory credit override for a completed dig, whoever put the block
	/// there; `dig_drop()` falls back to the block itself when diggable.
	pub drops: Option<BlockType>,
	/// Inventory credit override for the WORLD'S OWN copy of this block: it
	/// comes apart in your hands, while one somebody placed hands itself
	/// back (`dig_credit`). The difference between rock in a mountain and a
	/// block on a shelf.
	///
	/// This is what lets a substance the world is MADE of still be worked at
	/// the bench without opening a loop. Clean stone is craftable, and a
	/// natural one crumbles to Rubble — so the only clean stone a player can
	/// ever hold came off the bench, which is exactly what
	/// [`BlockType::is_bench_only`] tests and what makes its extraction pay
	/// no XP. Were a bench stone to crumble too, craft → place → dig →
	/// extract would launder a made block back into a world-given one and
	/// collect the XP for it.
	///
	/// Mutually exclusive with [`Self::drops`] (pinned by `table_sanity`):
	/// a row says either "always this" or "this, unless somebody made it".
	pub crumbles: Option<BlockType>,
	/// Sub-block shape family; `Some` ⟺ `class == RenderClass::Shaped`
	/// (pinned by `table_sanity`).
	pub shape: Option<ShapeKind>,
	/// Stage-A light (SKILLS-DESIGN §7.4): every face renders fullbright
	/// ink — the shader skips the sun/shadow/AO terms. Independent of
	/// [`Self::light`] on purpose: ore-reveal seams want ink that casts
	/// nothing, and a future recessed lamp could cast without glowing.
	///
	/// Both stay per-ROW builders while everything else about a §7.2 grid
	/// cell derives from the material × shape pair, so both are the one
	/// thing a second shape of a substance can silently disagree about.
	/// They may not: `every_shape_of_a_material_agrees_on_light` holds a
	/// material's shapes to one answer, which is what a FUNCTIONAL row
	/// (torch, lamp) opts out of by carrying no material at all.
	pub emissive: bool,
	/// The level this block CASTS, feeding the GI material LUT and
	/// `gi.rs`'s light list and nothing else (GI-DESIGN.md). Purely
	/// client-side data — what it feeds is derived, never serialized.
	/// Agrees across a material's shapes for the same reason
	/// [`Self::emissive`] does.
	pub light: u8,
	/// This block radiates from its exposed FACES, not from the middle of
	/// its cell — the difference between a lamp and a seam in a wall.
	///
	/// A torch is a point: it stands in open air and throws light every way
	/// at once, and one entry at its flame is the whole of it. A vein is
	/// not. Most of a vein is buried, and buried rock must light nothing —
	/// so a cell's worth of ore is a light only where it MEETS the air, and
	/// what it lights is the room in front of that face. Setting this makes
	/// the block's light a surface quantity: `gi::classify_chunk` declines
	/// to place a point source for it, and the mesher — which is already
	/// deciding, face by face, what is exposed — hands the renderer the
	/// merged rectangles instead (GI-DESIGN.md R31).
	///
	/// [`Self::light`] still says how bright, and the LUT still says in
	/// what colour. This says only WHERE FROM.
	pub surface_light: bool,
	/// Which per-material dig-speed curve applies (SKILLS-DESIGN §5.1).
	/// Set together with hardness by the `dig` builder so a diggable row
	/// can never forget it. Shared client/server behavior — a family
	/// change is a SIM_VERSION bump like a hardness retune.
	pub dig_family: DigFamily,
	/// The substance this block is made OF (SKILLS-DESIGN §7.2's grid).
	/// Together with [`Self::shape`] it derives the unlock level, the craft
	/// cost, the extraction yield and the earned half of the strength wall — see
	/// [`crate::material`]. `None` for air, the water family and every
	/// FUNCTIONAL item (tools, torches, lamps): function is priced by hand,
	/// never by volume, so those carry their own small tables instead.
	pub material: Option<Material>,
}

/// The per-material dig-speed families (SKILLS-DESIGN §5.1). A family is
/// just the two ENDPOINTS of its Mining curve — its first-level speed and
/// its mastery speed — so it chooses the approach and the ceiling and
/// nothing else; the shape between them is one shared taper in
/// `skills::dig_speed_permille`. It is also the axis the strength wall is
/// keyed on (`unlock::mine`), which is what opens every block of a family
/// together. `Fixed` is the zero-width range for things that must never
/// scale: near-instant foliage, fixtures, and the water scoop whose
/// duration is pinned to the viewmodel swing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DigFamily {
	/// Exactly the base duration at every level — never faster, never
	/// slower.
	Fixed,
	/// Loose ground: the gentlest climb, and diggable from the first swing.
	Soft,
	/// Timber: a firmer start and a gentler ceiling. The middle rung of the
	/// strength wall.
	Wood,
	/// Rock resists a novice hard, and is the last rung of the strength
	/// wall.
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
		crumbles: None,
		shape: None,
		emissive: false,
		light: 0,
		surface_light: false,
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

/// Diggable: hold-to-dig completes after `ticks` sim ticks scaled
/// by `family`'s Mining curve. `ticks` is the BASE the curve multiplies,
/// not the duration at some reference level — only the flat `Fixed`
/// family ever spends exactly it. Family rides the same builder as
/// hardness so a new diggable row can't forget it.
const fn dig(mut p: BlockProps, ticks: u16, family: DigFamily) -> BlockProps {
	p.hardness_ticks = ticks;
	p.dig_family = family;
	p
}

/// Inventory credit override: a completed dig lands `d`, not the block
/// itself, whoever put it there.
const fn drops(mut p: BlockProps, d: BlockType) -> BlockProps {
	p.drops = Some(d);
	p
}

/// The rock family's Rubble rule: the WORLD'S copy lands `d`, and one
/// somebody placed lands itself. See [`BlockProps::crumbles`] — this is
/// what keeps a substance the world is made of craftable without opening
/// a craft-and-extract loop.
const fn crumbles(mut p: BlockProps, d: BlockType) -> BlockProps {
	p.crumbles = Some(d);
	p
}

/// The substance half of SKILLS-DESIGN §7.2's grid. Everything a
/// structural block's economy needs — unlock level, craft cost, extraction
/// yield, the earned strength wall — falls out of this plus the row's `shaped` tag, so a
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

/// This block casts light at `level` — see [`BlockProps::light`].
const fn emits(mut p: BlockProps, level: u8) -> BlockProps {
	p.light = level;
	p
}

/// This block casts light at `level` from its exposed FACES rather than
/// from the middle of its cell — see [`BlockProps::surface_light`]. A
/// seam, not a lamp.
const fn seams(mut p: BlockProps, level: u8) -> BlockProps {
	p.light = level;
	p.surface_light = true;
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
		crumbles: None,
		shape: None,
		emissive: false,
		light: 0,
		surface_light: false,
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
	/// joinery (the bucket, the furniture, and the Lumber block with its
	/// stairs, slab and door); [`LUMBER_DARK`] and [`LUMBER_RED`] are the
	/// second and third timbers, each with a block set of its own. The
	/// dark one is also what [`TORCH_SIDE`]'s post is cut from, because
	/// that texture is drawn fullbright and a pale timber has no headroom
	/// left to take the emissive gain.
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
	/// §7.2's later materials and §7.3's two lamps.
	pub const STONE_BRICK: u16 = 27;
	pub const SANDSTONE_BRICK: u16 = 28;
	pub const POLISHED_STONE: u16 = 29;
	pub const GLASS: u16 = 30;
	pub const METAL: u16 = 31;
	pub const CRYSTAL: u16 = 32;
	pub const LANTERN: u16 = 33;
	pub const AETHERLAMP: u16 = 34;
	/// The two tool glows (§7.3), worn by the siphon and pick ROWS. Those
	/// blocks are never placed, so these two layers are read in exactly
	/// two places: the emissive face of the tool's authored geometry
	/// (`item_geom`), and the GI material LUT, which takes the colour of
	/// the light a held tool casts from its row's texture. Authoring the
	/// glow authors the light.
	pub const SIPHON_GLOW: u16 = 35;
	pub const PICK_GLOW: u16 = 36;
	/// The four SPENT vein faces — the same rock as their live tier, with
	/// the light gone out of the flecks. One per tier rather than the one
	/// grey [`DEPLETED_STONE`] they replace, because a tapped seam is still
	/// the rock it was: you should be able to tell at a glance which vein
	/// you worked, and the block you carry home from it is a trophy of that
	/// tier and no other.
	pub const DEPLETED_EMBERORE: u16 = 37;
	pub const DEPLETED_DUSKSTONE: u16 = 38;
	pub const DEPLETED_GLOWVEIN: u16 = 39;
	pub const DEPLETED_AETHEROCK: u16 = 40;
	/// The three surface-node faces and their spent siblings — the same
	/// live/spent pairing the ore tiers wear, in the hosts' own bodies
	/// (bark, stone, wet sand).
	pub const HEARTWOOD: u16 = 41;
	pub const DRAINED_HEARTWOOD: u16 = 42;
	pub const LEYSTONE: u16 = 43;
	pub const SPENT_LEYSTONE: u16 = 44;
	pub const SPRINGSTONE: u16 = 45;
	pub const SPENT_SPRINGSTONE: u16 = 46;

	/// Layers in the array, i.e. `digblock_assets::tex::LAYERS.len()`.
	/// Pinned against it by `every_block_texture_layer_is_uploaded` in
	/// digblock-render, the one crate that can see both.
	pub const COUNT: u16 = SPENT_SPRINGSTONE + 1;
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

	/// The level this block casts, 0 for anything that casts nothing —
	/// see [`BlockProps::light`].
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
	/// The state encoding is shared, so one predicate covers
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
	///
	/// Memoized, because this is not the cold lookup it reads as:
	/// `slab_base` is the one asking, and it sits on the slab GROW path
	/// that every place-against-a-slab runs through. A linear walk of the
	/// catalog with two table reads per row is a lot of work to learn that
	/// a wood slab completes into wood.
	pub fn from_grid(m: Material, s: ShapeFamily) -> Option<BlockType> {
		static GRID: std::sync::OnceLock<Vec<Option<BlockType>>> = std::sync::OnceLock::new();
		let grid = GRID.get_or_init(|| {
			let mut cells = vec![None; Material::COUNT * ShapeFamily::COUNT];
			// First authored row wins, matching the `find` this replaced.
			for b in BlockType::ALL {
				if let Some((bm, bs)) = b.grid_cell() {
					let slot = &mut cells[bm as usize * ShapeFamily::COUNT + bs as usize];
					if slot.is_none() {
						*slot = Some(b);
					}
				}
			}
			cells
		});
		grid[m as usize * ShapeFamily::COUNT + s as usize]
	}

	/// How much water this block holds — None for non-water.
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

	/// The flowing-water block for a below-source amount.
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
	/// worldgen upholds the same floor by never carving the bedrock course
	/// (see worldgen/caves.rs).
	#[inline]
	pub fn dig_hardness(self, y: i32) -> Option<u16> {
		if y <= BEDROCK_Y {
			return None;
		}
		self.hardness_ticks()
	}

	/// What digging the WORLD'S own copy of this block hands you: the
	/// `drops` or `crumbles` override, else the block itself. None for
	/// undiggable blocks.
	///
	/// The question every economy rule means — [`Self::is_bench_only`],
	/// [`crate::unlock::pickup`] and the no-matter-creation pins all ask
	/// what a fresh world can put in a player's hands, never what one
	/// particular cell will. That answer belongs to [`Self::dig_credit`],
	/// of which this is the pristine case.
	#[inline]
	pub fn dig_drop(self) -> Option<BlockType> {
		self.hardness_ticks()?;
		let p = self.props();
		Some(p.drops.or(p.crumbles).unwrap_or(self))
	}

	/// What a completed dig actually CREDITS: the world's own rock
	/// crumbles, and a cell somebody wrote hands back what they put in it.
	///
	/// `edited` is whether that coordinate has ever been written — the
	/// server reads its journal, the client its `EditOverlay` mirror of the
	/// same, exactly as [`crate::worldgen::ore::revealed`] does and for the
	/// same anti-dupe reason. An edited cell holding a `crumbles` block is
	/// one somebody PLACED, since nothing can be written into solid rock,
	/// so the block in it was made at the bench and comes back whole.
	///
	/// That is what closes the craft-and-extract loop on a substance the
	/// world is made of: a made stone can never launder itself back into
	/// the world-given Rubble that pays extraction XP. `alchemy` pins it as
	/// `no_one_who_can_make_a_block_can_dig_it_into_xp`.
	///
	/// `alchemy` is the DIGGER'S level, and the crumble stands for anyone
	/// who could not have made one — they are not undoing their own work,
	/// so they get the rubble and the XP that goes with it. The loop stays
	/// shut for a single player because the levels that can craft a block
	/// are exactly the levels it hands back to. Asked through
	/// [`crate::unlock::can_craft`] rather than compared here: no caller
	/// holds an unlock number.
	#[inline]
	pub fn dig_credit(self, edited: bool, alchemy: u32) -> Option<BlockType> {
		self.hardness_ticks()?;
		let p = self.props();
		if let Some(d) = p.drops {
			return Some(d);
		}
		match p.crumbles {
			Some(c) if !edited || !crate::unlock::can_craft(self, alchemy) => Some(c),
			_ => Some(self),
		}
	}

	/// Is the bench this block's only source — can no dig in an untouched
	/// world ever put one in a player's hands?
	///
	/// THE question the transmutation catalog is gated on: nothing the world
	/// hands you may be transmuted. The gate itself lives in
	/// `unlock`/`alchemy`; this is only the predicate.
	///
	/// Derived, never listed: bench-only means nothing the world PLACES
	/// crumbles or drops into it. That gets the awkward rows right without
	/// anyone remembering them — a bucket scoop credits Water through this
	/// same path so water falls out of the catalog on its own, a tool is
	/// never dug, and an ore credits itself and is correctly refused a
	/// recipe.
	///
	/// Strictly weaker than [`Self::is_crafted_only`], and the gap is the
	/// whole design: the world is MADE of stone but hands you Rubble, so
	/// clean stone is the one block the world places and never gives.
	///
	/// Memoized — a linear walk of the catalog per call, on a question that
	/// is a property of the block table.
	pub fn is_bench_only(self) -> bool {
		static BENCH: std::sync::OnceLock<Vec<bool>> = std::sync::OnceLock::new();
		let bench = BENCH.get_or_init(|| {
			let mut out = vec![true; BlockType::COUNT];
			for n in BlockType::ALL {
				if n.is_crafted_only() {
					continue;
				}
				if let Some(d) = n.dig_drop() {
					out[d as usize] = false;
				}
			}
			out
		});
		bench[self as usize]
	}

	/// Does holding this type claim a hotbar or inventory slot?
	///
	/// Two exceptions, and they are the same shape: a thing that exists in
	/// the world but is never an item you arrange.
	///
	/// **Water** — the only water a player can carry is the one charge in
	/// the water tool, and the tool has its own permanent slot to show it
	/// in. Its count still lives in the [`crate::inventory::Inventory`],
	/// which is where the charge is stored, synced and persisted.
	///
	/// **A live node** — a vein or a surface node is world geometry.
	/// Siphoning it pays essence straight into your stocks and leaves the
	/// kind's spent face standing, and the spent face is the block you
	/// break and carry (SKILLS-DESIGN §5.3). Nothing credits a node block,
	/// so nothing may hold one.
	///
	/// [`crate::inv_grid::reconcile_layout`] is the enforcer in both cases,
	/// which makes this the MIGRATION as well as the rule: a save written
	/// while water was a stack, or while a siphon handed out ore, drops
	/// those slots on the first reconcile rather than needing a version.
	#[inline]
	pub fn is_slottable(self) -> bool {
		self != BlockType::Water && !self.is_tool() && !self.is_node_block()
	}

	/// One of the LIVE node faces — an ore tier or a surface node, the
	/// blocks the reveal stamp paints into pristine host cells. None is
	/// ever an item.
	///
	/// The roster itself is [`crate::worldgen::nodes`]'s; this is the
	/// membership test the block layer needs without reaching into
	/// worldgen, and `node_roster_matches_the_block_predicate` pins the
	/// two against each other.
	#[inline]
	pub fn is_node_block(self) -> bool {
		self.node_flecks().is_some()
	}

	/// The fleck palette of a live node: the crystal's own colour, and the
	/// lighter rim that catches the light around it.
	///
	/// ONE authority for what a tier looks like. `texgen` paints the flecks
	/// from it and paints the spent rock's dead flecks from it too, and the
	/// GI material LUT casts the vein's light in the core colour — so a
	/// retint moves the rock, its tapped sibling and the light it throws
	/// together, and none of the three can drift.
	///
	/// It has to be stated rather than measured, and that is worth knowing
	/// before anyone tries to derive it: an emitter's tint is normally the
	/// mean of its texture, which works for a torch because a torch's
	/// texture is mostly flame. An ore texture is a wide grey field with a
	/// few flecks on it, so its mean is STONE — near enough colourless on
	/// every tier — and no weighting of that image recovers the hue, since
	/// the problem is the composition and not the average (GI-DESIGN.md
	/// R31).
	#[inline]
	pub const fn node_flecks(self) -> Option<([u8; 3], [u8; 3])> {
		Some(match self {
			BlockType::Emberore => ([214, 96, 34], [255, 176, 84]),
			BlockType::Duskstone => ([104, 78, 168], [172, 146, 236]),
			BlockType::Glowvein => ([72, 226, 214], [198, 255, 248]),
			BlockType::Aetherock => ([240, 214, 96], [255, 248, 200]),
			// The surface nodes wear their essence's own colour: living
			// green, earthen amber, spring blue.
			BlockType::Heartwood => ([118, 196, 84], [204, 255, 164]),
			BlockType::Leystone => ([198, 150, 70], [244, 208, 140]),
			BlockType::Springstone => ([62, 158, 224], [172, 224, 255]),
			_ => return None,
		})
	}

	/// Foliage: the trivial-break rows the dig award zeroes
	/// (`skills::dig_xp`) — kept as one named predicate so anything else
	/// that ever keys on "the trivial canopy set" shares the same roster.
	#[inline]
	pub fn is_foliage(self) -> bool {
		matches!(self, BlockType::Leaves | BlockType::PineLeaves)
	}

	/// A tool id: an item-only row that lives in the dedicated tool slot,
	/// never in a block slot or the grid, and never in the world.
	#[inline]
	pub fn is_tool(self) -> bool {
		matches!(
			self,
			BlockType::Bucket
				| BlockType::EssenceSiphon
				| BlockType::ResonantPick
				| BlockType::MastersSigil
		)
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
			// A row says either "always this" or "this, unless somebody
			// made it" — never both, or the unconditional one silently wins.
			assert!(
				b.props().drops.is_none() || b.props().crumbles.is_none(),
				"{b:?} sets both drops and crumbles"
			);
		}
	}

	/// Only the world's own rock crumbles, and it crumbles for everyone who
	/// could not have made one.
	///
	/// The two halves are one rule read from either end: a pristine cell is
	/// the mountain's whatever your level, and an edited one is somebody's
	/// work — returned whole to a mason, and broken by anyone who has not
	/// learned to cut it. `dig_drop` is the pristine answer and must stay
	/// level-blind, because every economy pin means "what can a fresh world
	/// put in a player's hands".
	#[test]
	fn only_the_worlds_own_rock_crumbles() {
		use crate::skills::MAX_LEVEL;
		for &b in &BlockType::ALL {
			let Some(into) = b.props().crumbles else {
				// Everything else answers the same however the cell got
				// there — the conditional arm is opt-in, one row at a time.
				for level in [1, MAX_LEVEL] {
					assert_eq!(b.dig_credit(false, level), b.dig_drop(), "{b:?}");
					assert_eq!(b.dig_credit(true, level), b.dig_drop(), "{b:?}");
				}
				continue;
			};
			assert_ne!(into, b, "{b:?} crumbles into itself, which says nothing");
			let made = crate::unlock::craft(b).expect("a crumbling block is bench work");
			for level in 1..=MAX_LEVEL {
				assert_eq!(
					b.dig_credit(false, level),
					Some(into),
					"{b:?} in the mountain crumbles at every level, {level} included"
				);
				let want = if level >= made { b } else { into };
				assert_eq!(
					b.dig_credit(true, level),
					Some(want),
					"a placed {b:?} dug at {level}"
				);
			}
			assert_eq!(b.dig_drop(), Some(into), "{b:?} — the pristine answer");
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
			(BlockType::StoneBrick, Rock),
			(BlockType::StoneBrickStairs, Rock),
			(BlockType::StoneBrickSlab, Rock),
			(BlockType::StoneBrickDoor, Rock),
			(BlockType::PolishedStone, Rock),
			(BlockType::PolishedStoneStairs, Rock),
			(BlockType::PolishedStoneSlab, Rock),
			(BlockType::SandstoneBrick, Rock),
			(BlockType::SandstoneBrickStairs, Rock),
			(BlockType::SandstoneBrickSlab, Rock),
			(BlockType::Glass, Soft),
			(BlockType::Metal, Rock),
			(BlockType::MetalStairs, Rock),
			(BlockType::MetalSlab, Rock),
			(BlockType::MetalDoor, Rock),
			(BlockType::Crystal, Rock),
			(BlockType::CrystalSlab, Rock),
			(BlockType::Lantern, Rock),
			(BlockType::Aetherlamp, Rock),
			(BlockType::DepletedEmberore, Rock),
			(BlockType::DepletedDuskstone, Rock),
			(BlockType::DepletedGlowvein, Rock),
			(BlockType::DepletedAetherock, Rock),
			// Surface nodes dig as their HOST digs: heartwood is timber,
			// leystone is rock, a spring bed is loose ground — the node
			// changes what a hold pays, never what family swings at it.
			(BlockType::Heartwood, Wood),
			(BlockType::DrainedHeartwood, Wood),
			(BlockType::Leystone, Rock),
			(BlockType::SpentLeystone, Rock),
			(BlockType::Springstone, Soft),
			(BlockType::SpentSpringstone, Soft),
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
	/// cost, its extraction yield and its earned wall all at once, so a new
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
			(B::DepletedEmberore, M::DepletedEmberore),
			(B::DepletedDuskstone, M::DepletedDuskstone),
			(B::DepletedGlowvein, M::DepletedGlowvein),
			(B::DepletedAetherock, M::DepletedAetherock),
			(B::Heartwood, M::Heartwood),
			(B::DrainedHeartwood, M::DrainedHeartwood),
			(B::Leystone, M::Leystone),
			(B::SpentLeystone, M::SpentLeystone),
			(B::Springstone, M::Springstone),
			(B::SpentSpringstone, M::SpentSpringstone),
			(B::StoneBrick, M::StoneBrick),
			(B::StoneBrickStairs, M::StoneBrick),
			(B::StoneBrickSlab, M::StoneBrick),
			(B::StoneBrickDoor, M::StoneBrick),
			(B::PolishedStone, M::PolishedStone),
			(B::PolishedStoneStairs, M::PolishedStone),
			(B::PolishedStoneSlab, M::PolishedStone),
			(B::SandstoneBrick, M::SandstoneBrick),
			(B::SandstoneBrickStairs, M::SandstoneBrick),
			(B::SandstoneBrickSlab, M::SandstoneBrick),
			(B::Glass, M::Glass),
			(B::Metal, M::Metal),
			(B::MetalStairs, M::Metal),
			(B::MetalSlab, M::Metal),
			(B::MetalDoor, M::Metal),
			(B::Crystal, M::Crystal),
			(B::CrystalSlab, M::Crystal),
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
				B::EssenceSiphon,
				B::ResonantPick,
				B::Lantern,
				B::Aetherlamp,
				B::MastersSigil,
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

	/// Light is a property of the SUBSTANCE, not of the piece it was cut
	/// into: a crystal slab is the same crystal as the cube it came off,
	/// so it wears the same ink and casts the same level.
	///
	/// This is the grid's one un-derived pair. The unlock level, the craft
	/// cost, the extraction yield, the earned wall and the transmute family
	/// all fall out of material × shape, but `emissive` and `light` stayed
	/// per-ROW builders — and a per-row property on a grid axis drifts the
	/// moment a material grows a second shape. It did: `Crystal` glowed and
	/// `CrystalSlab` did not, so growing a slab stack through its third
	/// third (`slab_grown` → `slab_base`) snapped the identical volume of
	/// the identical stuff to fullbright in one frame.
	///
	/// The two properties are pinned SEPARATELY because they are genuinely
	/// independent (see [`BlockProps::emissive`]): a deep seam wears ink
	/// and casts nothing, and a recessed lamp could cast without glowing.
	/// What neither may do is differ between a cube and its own slab.
	///
	/// FUNCTIONAL rows are outside this by construction rather than by
	/// exemption — the torch and the two lamps carry no material at all
	/// (`material_roster_is_pinned` spells that set out), so they are
	/// hand-authored light with no sibling shape to agree with.
	#[test]
	fn every_shape_of_a_material_agrees_on_light() {
		let mut compared = 0;
		for &m in &Material::ALL {
			let mut rows = BlockType::ALL
				.into_iter()
				.filter(|b| b.material() == Some(m));
			let first = rows
				.next()
				.unwrap_or_else(|| panic!("{m:?} has no row at all"));
			for b in rows {
				assert_eq!(
					b.props().emissive,
					first.props().emissive,
					"{b:?} and {first:?} are both {m:?} but disagree on the ink"
				);
				assert_eq!(
					b.props().light,
					first.props().light,
					"{b:?} and {first:?} are both {m:?} but cast different light"
				);
				compared += 1;
			}
		}
		// A walk that compares nothing passes forever. Every row carrying a
		// material is either its material's first shape or one comparison,
		// so this pins that the loop actually reached all of them.
		let rows = BlockType::ALL
			.into_iter()
			.filter(|b| b.material().is_some())
			.count();
		assert_eq!(
			compared + Material::COUNT,
			rows,
			"{compared} comparisons over {rows} rows in {} materials",
			Material::COUNT
		);
		use BlockType as B;
		// The set that actually exercises the rule, named so re-authoring
		// the crystal is a deliberate edit rather than a silent one.
		assert!(B::Crystal.props().emissive && B::CrystalSlab.props().emissive);
		assert_eq!(
			B::Crystal.props().light,
			0,
			"ink only — a wall lights nothing"
		);
		assert_eq!(B::CrystalSlab.props().light, 0);
		// And the counter-examples, on the other side of the material line.
		for b in [B::Torch, B::Lantern, B::Aetherlamp] {
			assert_eq!(b.material(), None, "{b:?} is functional, so hand-authored");
			assert!(b.props().emissive && b.props().light > 0, "{b:?} lights");
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
