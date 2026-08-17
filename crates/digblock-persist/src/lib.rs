//! Local-world persistence shared by the dedicated server and the
//! client's named saves: the journal format (`journal`), and the
//! `WorldStore` seam the engine talks to (desktop: `FsWorldStore`;
//! web: a localStorage impl in apps/web — this crate's wasm build is
//! IO-free).

pub mod b64;
#[cfg(not(target_arch = "wasm32"))]
mod fs_store;
pub mod journal;

#[cfg(not(target_arch = "wasm32"))]
pub use fs_store::FsWorldStore;

/// Re-exported for store impls that don't depend on core directly.
pub use digblock_core::overlay::EditOverlay;

/// One saved world in the list.
#[derive(Clone, Debug)]
pub struct WorldMeta {
	/// Stable opaque key (directory / storage-key name).
	pub id: String,
	pub name: String,
	pub seed: f64,
	pub created_ms: u64,
	pub last_played_ms: u64,
	/// False when the journal's format/worldgen version doesn't match:
	/// listed (dim, delete-only) but refused for play — a version bump
	/// must never silently wipe a named save.
	pub compatible: bool,
}

/// Player state restored on re-entry (never sent anywhere).
#[derive(Clone, Copy, Debug)]
pub struct PlayerPersist {
	pub pos: [f32; 3],
	pub yaw: f32,
	pub pitch: f32,
}

/// The player-record revision — CONTENT migrations, not format ones (the
/// journal's `VERSION` stays the format/worldgen contract, and bumping it
/// refuses a client save outright, which is far too blunt for handing
/// someone a bucket). A store stamps the current revision on every write
/// and reports a FRESH world (no state written yet) as current, so the
/// only saves below it are ones actually written by an older build.
///
/// rev 1: the bucket joined the catalog (SKILLS-DESIGN §13.5). A rev-0
/// save was written when every player owned one, so the engine grants it
/// on load — a migration, never a windfall, because a fresh world starts
/// at the current rev and gets nothing.
pub const SAVE_REV: u32 = 1;

/// Everything needed to enter a saved world.
#[derive(Debug)]
pub struct WorldSave {
	pub meta: WorldMeta,
	/// The [`SAVE_REV`] the state was written at (0 = before revisions
	/// existed).
	pub rev: u32,
	/// Final cell value per edited cell (the LWW index, flattened).
	pub edits: Vec<((i32, i32, i32), u16)>,
	/// Inventory as wire pairs (block id, count).
	pub inventory: Vec<(u16, u32)>,
	/// Hotbar slot assignments in `digblock_core::hotbar` wire encoding
	/// (one id per slot). Empty = legacy save with no arrangement.
	pub hotbar: Vec<u16>,
	/// Inventory-grid slot assignments, same wire encoding. Empty =
	/// legacy save (the engine derives the arrangement).
	pub grid: Vec<u16>,
	/// Per-skill XP totals as `(skill index, total)` pairs. Empty =
	/// pre-skills save (every skill derives at its base level; nothing
	/// else is touched).
	pub skills: Vec<(u8, u64)>,
	pub player: Option<PlayerPersist>,
}

/// The engine's injected persistence backend for named local worlds.
/// All methods are synchronous and must degrade (log, keep running)
/// rather than fail the game; `open_world`/`create_world` return errors
/// for the menu's status line.
///
/// `maintain`/`flush` receive the live edit overlay by reference so a
/// snapshot-based backend (web) can re-serialize the authoritative
/// state without mirroring every append; the file backend appends
/// per-edit and ignores it.
pub trait WorldStore {
	fn list_worlds(&mut self) -> Vec<WorldMeta>;
	fn create_world(&mut self, name: &str, seed: f64) -> Result<WorldMeta, String>;
	fn delete_world(&mut self, id: &str);
	/// Open `id` and make it the active world (closing any previous).
	fn open_world(&mut self, id: &str) -> Result<WorldSave, String>;
	/// One accepted local edit (dig/place/fluid), full cell value.
	fn append_edit(&mut self, x: i32, y: i32, z: i32, cell: u16);
	/// Latest inventory + hotbar/grid arrangements (wire-encoded slots)
	/// + skill XP totals + player state (throttled to disk internally).
	fn save_player(
		&mut self,
		inventory: &[(u16, u32)],
		hotbar: &[u16],
		grid: &[u16],
		skills: &[(u8, u64)],
		player: &PlayerPersist,
	);
	/// Per-frame upkeep: bounded flush/compaction work.
	fn maintain(&mut self, live_edits: Option<&EditOverlay>);
	/// Synchronous full flush (suspend, leaving the world).
	fn flush(&mut self, live_edits: Option<&EditOverlay>);
	/// Flush and forget the active world.
	fn close_world(&mut self);
}
