//! Web `WorldStore`: named saves in localStorage. The journal is stored
//! as base64 of the exact server-format snapshot bytes
//! (`digblock_persist::journal::snapshot_bytes`), so version/seed
//! checks and LWW replay are the shared code paths. localStorage over
//! IndexedDB deliberately: `flush` runs inside the synchronous
//! `on_suspend` contract, and after LWW compaction a save is live-cells
//! small (~21 bytes/cell base64), comfortably inside the quota.
//!
//! The store never mirrors the live index — `maintain`/`flush` receive
//! the Pipeline's authoritative `EditOverlay` and re-serialize it on a
//! 5s-if-dirty throttle. Quota/private-mode failures degrade to
//! in-memory play with a one-time status the menu surfaces.

use digblock_persist::journal::{parse_snapshot, snapshot_bytes, JournalError, JournalIndex};
use digblock_persist::{EditOverlay, PlayerPersist, WorldMeta, WorldSave, WorldStore};

const INDEX_KEY: &str = "digblock.worlds";
/// Journal snapshot cadence when dirty, ms.
const WRITE_INTERVAL_MS: f64 = 5000.0;

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
	inventory: Vec<(u16, u32)>,
	/// Hotbar slots, core wire encoding; empty in pre-arrangement saves.
	hotbar: Vec<u16>,
	/// Inventory-grid slots, same encoding; empty in older saves.
	grid: Vec<u16>,
	player: Option<([f32; 3], f32, f32)>,
}

struct Active {
	id: String,
	seed: f64,
	/// Edits landed since the last snapshot write.
	dirty: bool,
	pending_state: Option<StateFile>,
	last_write_ms: f64,
	/// Storage rejected a write (quota/private mode): stop retrying
	/// every interval, surface once.
	degraded: bool,
}

pub struct LocalStorageWorldStore {
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
	pub fn new() -> Self {
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

	fn journal_key(id: &str) -> String {
		format!("digblock.world.{id}.journal")
	}

	fn state_key(id: &str) -> String {
		format!("digblock.world.{id}.state")
	}

	fn read_blob(&self, key: &str) -> Option<Vec<u8>> {
		self.storage
			.as_ref()
			.and_then(|s| s.get_item(key).ok().flatten())
			.and_then(|b64| digblock_persist::b64::decode(&b64))
	}

	/// Serialize the live overlay as the world's journal snapshot.
	fn write_snapshot(&mut self, live: &EditOverlay) {
		let Some(active) = &mut self.active else {
			return;
		};
		let mut index = JournalIndex::default();
		for (x, y, z, cell) in live.iter() {
			index.record(digblock_net::messages::BlockEdit {
				x,
				y,
				z,
				block: cell,
			});
		}
		let bytes = snapshot_bytes(active.seed, &index);
		let key = Self::journal_key(&active.id);
		let ok = self.storage.as_ref().is_some_and(|s| {
			s.set_item(&key, &digblock_persist::b64::encode(&bytes))
				.is_ok()
		});
		if ok {
			active.dirty = false;
		} else if !active.degraded {
			active.degraded = true;
			log::warn!("world save write refused — playing from memory only");
		}
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
				// header version = listed but refused.
				let compatible = match self.read_blob(&Self::journal_key(&e.id)) {
					None => true,
					Some(bytes) => !matches!(
						parse_snapshot(&bytes, seed),
						Err(JournalError::WrongVersion(_))
					),
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
		out.sort_by(|a, b| b.last_played_ms.cmp(&a.last_played_ms));
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
		if let Some(s) = &self.storage {
			let _ = s.remove_item(&Self::journal_key(id));
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
		let edits = match self.read_blob(&Self::journal_key(id)) {
			None => Vec::new(), // created, never entered
			Some(bytes) => match parse_snapshot(&bytes, seed) {
				Ok(index) => index.iter_edits().collect(),
				Err(JournalError::WrongVersion(v)) => {
					return Err(format!("save is from an old version (v{v})"));
				}
				Err(e) => return Err(format!("save unreadable: {e}")),
			},
		};
		let state: StateFile = self
			.storage
			.as_ref()
			.and_then(|s| s.get_item(&Self::state_key(id)).ok().flatten())
			.and_then(|text| serde_json::from_str(&text).ok())
			.unwrap_or_default();
		self.stamp_last_played(id);
		self.active = Some(Active {
			id: id.to_string(),
			seed,
			dirty: false,
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
			edits,
			inventory: state.inventory,
			hotbar: state.hotbar,
			grid: state.grid,
			player: state
				.player
				.map(|(pos, yaw, pitch)| PlayerPersist { pos, yaw, pitch }),
		})
	}

	fn append_edit(&mut self, _x: i32, _y: i32, _z: i32, _cell: u16) {
		// The live overlay is authoritative; just note there's news.
		if let Some(active) = &mut self.active {
			active.dirty = true;
		}
	}

	fn save_player(
		&mut self,
		inventory: &[(u16, u32)],
		hotbar: &[u16],
		grid: &[u16],
		player: &PlayerPersist,
	) {
		if let Some(active) = &mut self.active {
			active.pending_state = Some(StateFile {
				inventory: inventory.to_vec(),
				hotbar: hotbar.to_vec(),
				grid: grid.to_vec(),
				player: Some((player.pos, player.yaw, player.pitch)),
			});
		}
	}

	fn maintain(&mut self, live_edits: Option<&EditOverlay>) {
		let now = js_sys::Date::now();
		let Some(a) = &mut self.active else {
			return;
		};
		let idle = !a.dirty && a.pending_state.is_none();
		if a.degraded || idle || now - a.last_write_ms < WRITE_INTERVAL_MS {
			return;
		}
		a.last_write_ms = now;
		let dirty = a.dirty;
		if dirty {
			if let Some(live) = live_edits {
				self.write_snapshot(live);
			}
		}
		self.write_state();
	}

	fn flush(&mut self, live_edits: Option<&EditOverlay>) {
		if self.active.as_ref().is_some_and(|a| a.dirty) {
			if let Some(live) = live_edits {
				self.write_snapshot(live);
			}
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
		if let Some(a) = &self.active {
			if a.dirty {
				log::warn!("world {} closed with unflushed edits", a.id);
			}
		}
		self.active = None;
	}
}
