//! Web `WorldStore`: named saves in localStorage, sharded by REGION. Each shard
//! is base64 of exact server-format journal bytes, so version/seed checks and
//! LWW replay are the shared code paths and a shard is just an ordinary journal
//! that holds one region. localStorage over IndexedDB deliberately: `flush` runs
//! inside the synchronous `on_suspend` contract, and after LWW compaction a save
//! is live-cells small.
//!
//! Sharding is why the save cost is bounded. A single blob meant every write
//! re-serialized the world's ENTIRE edit history, base64'd it and handed it to a
//! synchronous `setItem` — inside the frame, growing forever with the age of the
//! save. Now only regions that actually changed are rewritten.
//!
//! The store never mirrors the live index: `maintain`/`flush` receive the
//! Pipeline's authoritative `EditOverlay` and a region is serialized by asking
//! the overlay for that region's columns — O(region), not O(world). Only the
//! DIRTY-REGION SET is tracked here. Quota failures degrade to in-memory play
//! with a one-time status the menu surfaces.

use digblock_persist::journal::{
	JournalError, JournalIndex, REGION_COLS, merge_snapshot, region_of_block, region_snapshot_bytes,
};
use digblock_persist::{EditOverlay, PlayerPersist, WorldMeta, WorldSave, WorldStore};
use std::collections::HashSet;

const INDEX_KEY: &str = "digblock.worlds";
/// Journal snapshot cadence when dirty, ms.
const WRITE_INTERVAL_MS: f64 = 5000.0;
/// Region shards rewritten per `maintain`. Each is a synchronous
/// `setItem` on the main thread, so the throttled path stays bounded even
/// when a wide-area edit (a big fluid spill) dirties many regions at once;
/// `flush` ignores this because `on_suspend` must leave nothing behind.
const REGIONS_PER_MAINTAIN: usize = 2;

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct MetaEntry {
	id: String,
	name: String,
	/// Seed as exact bit pattern (hex) — the fs store's convention.
	seed_bits: String,
	created_ms: u64,
	last_played_ms: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct StateFile {
	/// `digblock_persist::SAVE_REV` at write time; absent (0) in saves
	/// from before revisions existed.
	rev: u32,
	inventory: Vec<(u16, u32)>,
	/// Hotbar slots, core wire encoding; empty in pre-arrangement saves.
	hotbar: Vec<u16>,
	/// Inventory-grid slots, same encoding; empty in older saves.
	grid: Vec<u16>,
	/// Per-skill XP totals; empty in pre-skills saves (level 1).
	skills: Vec<(u8, u64)>,
	player: Option<([f32; 3], f32, f32)>,
}

struct Active {
	id: String,
	seed: f64,
	/// Regions edited since their shard was last written. A region is
	/// cleared only once its bytes are committed, so a refused write
	/// retries rather than silently dropping the edits.
	dirty: HashSet<(i32, i32)>,
	pending_state: Option<StateFile>,
	last_write_ms: f64,
	/// Storage rejected a write (quota/private mode): stop retrying
	/// every interval, surface once.
	degraded: bool,
}

pub(crate) struct LocalStorageWorldStore {
	storage: Option<web_sys::Storage>,
	active: Option<Active>,
	id_counter: u64,
}

impl Default for LocalStorageWorldStore {
	fn default() -> Self {
		Self::new()
	}
}

impl LocalStorageWorldStore {
	pub(crate) fn new() -> Self {
		Self {
			storage: web_sys::window().and_then(|w| w.local_storage().ok().flatten()),
			active: None,
			id_counter: 0,
		}
	}

	fn read_index(&self) -> Vec<MetaEntry> {
		self.storage
			.as_ref()
			.and_then(|s| s.get_item(INDEX_KEY).ok().flatten())
			.and_then(|text| serde_json::from_str(&text).ok())
			.unwrap_or_default()
	}

	fn write_index(&self, index: &[MetaEntry]) -> Result<(), String> {
		let storage = self.storage.as_ref().ok_or("storage unavailable")?;
		let json = serde_json::to_string(index).map_err(|e| e.to_string())?;
		storage
			.set_item(INDEX_KEY, &json)
			.map_err(|_| "storage write refused (quota or private mode)".into())
	}

	/// Pre-sharding single-blob key. Still read (and migrated away from) at
	/// open; never written.
	fn legacy_journal_key(id: &str) -> String {
		format!("digblock.world.{id}.journal")
	}

	fn region_prefix(id: &str) -> String {
		format!("digblock.world.{id}.r.")
	}

	fn region_key(id: &str, (rx, rz): (i32, i32)) -> String {
		format!("digblock.world.{id}.r.{rx}_{rz}")
	}

	fn state_key(id: &str) -> String {
		format!("digblock.world.{id}.state")
	}

	/// Every localStorage key under a prefix. localStorage has no prefix
	/// scan, so this walks the whole keyspace — fine at open/delete time,
	/// never on the save path.
	fn keys_with_prefix(&self, prefix: &str) -> Vec<String> {
		let Some(s) = self.storage.as_ref() else {
			return Vec::new();
		};
		let n = s.length().unwrap_or(0);
		(0..n)
			.filter_map(|i| s.key(i).ok().flatten())
			.filter(|k| k.starts_with(prefix))
			.collect()
	}

	fn read_blob(&self, key: &str) -> Option<Vec<u8>> {
		self.storage
			.as_ref()
			.and_then(|s| s.get_item(key).ok().flatten())
			.and_then(|b64| digblock_persist::b64::decode(&b64))
	}

	/// Collect one region's cells out of the live overlay. Reads only the
	/// region's own columns (`REGION_COLS²` lookups), so the cost is the
	/// region's size, not the world's.
	fn region_index(live: &EditOverlay, region: (i32, i32)) -> JournalIndex {
		let mut index = JournalIndex::default();
		let (rx, rz) = region;
		for dz in 0..REGION_COLS {
			for dx in 0..REGION_COLS {
				let (cx, cz) = (rx * REGION_COLS + dx, rz * REGION_COLS + dz);
				let Some(cells) = live.column(cx, cz) else {
					continue;
				};
				for (&(x, y, z), &cell) in cells {
					index.record(digblock_net::messages::BlockEdit {
						x,
						y,
						z,
						block: cell,
					});
				}
			}
		}
		index
	}

	/// Rewrite one region's shard. Returns false if storage refused.
	fn write_region(&mut self, live: &EditOverlay, region: (i32, i32)) -> bool {
		let Some(active) = self.active.as_ref() else {
			return false;
		};
		let (seed, id) = (active.seed, active.id.clone());
		let index = Self::region_index(live, region);
		let key = Self::region_key(&id, region);
		let ok = match self.storage.as_ref() {
			None => false,
			// A region emptied by digging everything back has no cells
			// left; drop the shard rather than storing an empty blob.
			Some(s) if index.live_cells() == 0 => s.remove_item(&key).is_ok(),
			Some(s) => {
				let bytes = region_snapshot_bytes(seed, &index, region);
				s.set_item(&key, &digblock_persist::b64::encode(&bytes))
					.is_ok()
			}
		};
		let Some(active) = self.active.as_mut() else {
			return ok;
		};
		if ok {
			active.dirty.remove(&region);
		} else if !active.degraded {
			active.degraded = true;
			log::warn!("world save write refused — playing from memory only");
		}
		ok
	}

	/// Write up to `limit` dirty regions (all of them when None).
	fn write_dirty(&mut self, live: &EditOverlay, limit: Option<usize>) {
		let Some(active) = self.active.as_ref() else {
			return;
		};
		let mut todo: Vec<(i32, i32)> = active.dirty.iter().copied().collect();
		// Stable order so a partial write makes deterministic progress
		// instead of re-picking the same region out of a hash set.
		todo.sort_unstable();
		if let Some(n) = limit {
			todo.truncate(n);
		}
		for region in todo {
			if !self.write_region(live, region) {
				break;
			}
		}
	}

	/// Read every shard of a save into one index, migrating a pre-sharding
	/// single blob if it is still there.
	fn load_index(&mut self, id: &str, seed: f64) -> Result<JournalIndex, JournalError> {
		let mut index = JournalIndex::default();
		let mut found = false;
		for key in self.keys_with_prefix(&Self::region_prefix(id)) {
			let Some(bytes) = self.read_blob(&key) else {
				continue;
			};
			merge_snapshot(&bytes, seed, &mut index)?;
			found = true;
		}
		// Migration: an unsharded save from before this layout. Its bytes
		// are the same format, so it merges in directly; it is only split
		// on the way back out. The legacy key is dropped ONLY after every
		// shard is committed, so an interrupted migration re-runs from the
		// intact original instead of losing the world.
		if let Some(bytes) = self.read_blob(&Self::legacy_journal_key(id)) {
			merge_snapshot(&bytes, seed, &mut index)?;
			index.mark_all_dirty();
			let regions: Vec<(i32, i32)> = index.regions().collect();
			let n = regions.len();
			let mut all_ok = true;
			for region in regions {
				let bytes = region_snapshot_bytes(seed, &index, region);
				let ok = self.storage.as_ref().is_some_and(|s| {
					s.set_item(
						&Self::region_key(id, region),
						&digblock_persist::b64::encode(&bytes),
					)
					.is_ok()
				});
				all_ok &= ok;
			}
			if all_ok {
				if let Some(s) = &self.storage {
					let _ = s.remove_item(&Self::legacy_journal_key(id));
				}
				index.clear_all_dirty();
				log::info!("world {id}: migrated save to {n} region shards");
			} else {
				log::warn!("world {id}: shard migration incomplete — keeping the old save");
			}
			found = true;
		}
		let _ = found;
		Ok(index)
	}

	fn write_state(&mut self) {
		let Some(active) = &mut self.active else {
			return;
		};
		let Some(state) = active.pending_state.take() else {
			return;
		};
		let ok = serde_json::to_string(&state).is_ok_and(|json| {
			self.storage
				.as_ref()
				.is_some_and(|s| s.set_item(&Self::state_key(&active.id), &json).is_ok())
		});
		if !ok {
			active.pending_state = Some(state);
		}
	}

	fn stamp_last_played(&self, id: &str) {
		let mut index = self.read_index();
		if let Some(e) = index.iter_mut().find(|e| e.id == id) {
			e.last_played_ms = js_sys::Date::now() as u64;
			let _ = self.write_index(&index);
		}
	}
}

impl WorldStore for LocalStorageWorldStore {
	fn list_worlds(&mut self) -> Vec<WorldMeta> {
		let mut out: Vec<WorldMeta> = self
			.read_index()
			.into_iter()
			.map(|e| {
				let seed = u64::from_str_radix(&e.seed_bits, 16)
					.map(f64::from_bits)
					.unwrap_or(0.0);
				// Absent blob (never entered) = compatible; a mismatched
				// header version = listed but refused. One shard is enough
				// to judge: they share a header.
				let probe = self
					.keys_with_prefix(&Self::region_prefix(&e.id))
					.into_iter()
					.next()
					.unwrap_or_else(|| Self::legacy_journal_key(&e.id));
				let compatible = match self.read_blob(&probe) {
					None => true,
					Some(bytes) => {
						let mut sink = JournalIndex::default();
						!matches!(
							merge_snapshot(&bytes, seed, &mut sink),
							Err(JournalError::WrongVersion(_))
						)
					}
				};
				WorldMeta {
					id: e.id,
					name: e.name,
					seed,
					created_ms: e.created_ms,
					last_played_ms: e.last_played_ms,
					compatible,
				}
			})
			.collect();
		out.sort_by_key(|w| std::cmp::Reverse(w.last_played_ms));
		out
	}

	fn create_world(&mut self, name: &str, seed: f64) -> Result<WorldMeta, String> {
		self.id_counter += 1;
		let id = format!(
			"{:016x}",
			(js_sys::Date::now() as u64) ^ (self.id_counter << 48)
		);
		let now = js_sys::Date::now() as u64;
		let mut index = self.read_index();
		index.push(MetaEntry {
			id: id.clone(),
			name: name.to_string(),
			seed_bits: format!("{:016x}", seed.to_bits()),
			created_ms: now,
			last_played_ms: now,
		});
		self.write_index(&index)?;
		Ok(WorldMeta {
			id,
			name: name.to_string(),
			seed,
			created_ms: now,
			last_played_ms: now,
			compatible: true,
		})
	}

	fn delete_world(&mut self, id: &str) {
		if self.active.as_ref().is_some_and(|a| a.id == id) {
			self.active = None;
		}
		let index: Vec<MetaEntry> = self
			.read_index()
			.into_iter()
			.filter(|e| e.id != id)
			.collect();
		let _ = self.write_index(&index);
		for key in self.keys_with_prefix(&Self::region_prefix(id)) {
			if let Some(s) = &self.storage {
				let _ = s.remove_item(&key);
			}
		}
		if let Some(s) = &self.storage {
			let _ = s.remove_item(&Self::legacy_journal_key(id));
			let _ = s.remove_item(&Self::state_key(id));
		}
	}

	fn open_world(&mut self, id: &str) -> Result<WorldSave, String> {
		self.close_world();
		let entry = self
			.read_index()
			.into_iter()
			.find(|e| e.id == id)
			.ok_or("save metadata missing")?;
		let seed = u64::from_str_radix(&entry.seed_bits, 16)
			.map(f64::from_bits)
			.map_err(|_| "save metadata unreadable".to_string())?;
		let edits = match self.load_index(id, seed) {
			Ok(index) => index.iter_edits().collect(),
			Err(JournalError::WrongVersion(v)) => {
				return Err(format!("save is from an old version (v{v})"));
			}
			Err(e) => return Err(format!("save unreadable: {e}")),
		};
		let state: Option<StateFile> = self
			.storage
			.as_ref()
			.and_then(|s| s.get_item(&Self::state_key(id)).ok().flatten())
			.and_then(|text| serde_json::from_str(&text).ok());
		// A world with no state yet is FRESH, not legacy: it reports the
		// current revision so no migration ever fires on it.
		let rev = state.as_ref().map_or(digblock_persist::SAVE_REV, |s| s.rev);
		let state = state.unwrap_or_default();
		self.stamp_last_played(id);
		self.active = Some(Active {
			id: id.to_string(),
			seed,
			dirty: HashSet::new(),
			pending_state: None,
			last_write_ms: js_sys::Date::now(),
			degraded: false,
		});
		Ok(WorldSave {
			meta: WorldMeta {
				id: id.to_string(),
				name: entry.name,
				seed,
				created_ms: entry.created_ms,
				last_played_ms: entry.last_played_ms,
				compatible: true,
			},
			rev,
			edits,
			inventory: state.inventory,
			hotbar: state.hotbar,
			grid: state.grid,
			skills: state.skills,
			player: state
				.player
				.map(|(pos, yaw, pitch)| PlayerPersist { pos, yaw, pitch }),
		})
	}

	fn append_edit(&mut self, x: i32, _y: i32, z: i32, _cell: u16) {
		// The live overlay stays authoritative for CONTENT; all this needs
		// to know is which shard the news landed in.
		if let Some(active) = &mut self.active {
			active.dirty.insert(region_of_block(x, z));
		}
	}

	fn save_player(
		&mut self,
		inventory: &[(u16, u32)],
		hotbar: &[u16],
		grid: &[u16],
		skills: &[(u8, u64)],
		player: &PlayerPersist,
	) {
		if let Some(active) = &mut self.active {
			active.pending_state = Some(StateFile {
				rev: digblock_persist::SAVE_REV,
				inventory: inventory.to_vec(),
				hotbar: hotbar.to_vec(),
				grid: grid.to_vec(),
				skills: skills.to_vec(),
				player: Some((player.pos, player.yaw, player.pitch)),
			});
		}
	}

	fn maintain(&mut self, live_edits: Option<&EditOverlay>) {
		let now = js_sys::Date::now();
		let Some(a) = &mut self.active else {
			return;
		};
		let idle = a.dirty.is_empty() && a.pending_state.is_none();
		if a.degraded || idle || now - a.last_write_ms < WRITE_INTERVAL_MS {
			return;
		}
		a.last_write_ms = now;
		if let Some(live) = live_edits {
			self.write_dirty(live, Some(REGIONS_PER_MAINTAIN));
		}
		self.write_state();
	}

	fn flush(&mut self, live_edits: Option<&EditOverlay>) {
		// No region cap here: `on_suspend` may be the last callback the
		// process ever gets, so everything dirty has to land now.
		if let Some(live) = live_edits {
			self.write_dirty(live, None);
		}
		if let Some(active) = &mut self.active {
			active.last_write_ms = js_sys::Date::now();
		}
		self.write_state();
	}

	fn close_world(&mut self) {
		// The engine flushes with the live overlay before closing (see
		// Engine::close_local_save); anything still dirty here has no
		// overlay to serialize from, so just detach.
		self.write_state();
		if let Some(a) = &self.active
			&& !a.dirty.is_empty()
		{
			log::warn!(
				"world {} closed with {} unflushed region(s)",
				a.id,
				a.dirty.len()
			);
		}
		self.active = None;
	}
}
