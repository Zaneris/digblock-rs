//! Desktop `WorldStore`: one directory per save under an injected root
//! (`<data-dir>/digblock/worlds/<id>/`) holding `meta.json`, the world
//! journal (the exact server layout — a `world-<seedbits>/` directory of
//! per-region shards; `Journal` does the legacy-file migration, boot
//! compaction and torn-tail recovery for free), and `state.json`
//! (inventory + player), written atomically on a 5s-if-dirty throttle
//! (the server's InventoryStore cadence).

use crate::journal::{world_version, Journal};
use crate::{PlayerPersist, WorldMeta, WorldSave, WorldStore};
use digblock_core::overlay::EditOverlay;
use digblock_net::messages::BlockEdit;
use digblock_net::protocol::SERVER_ID;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Player/inventory snapshot cadence (dirty-gated).
const STATE_WRITE_SECS: u64 = 5;

#[derive(serde::Serialize, serde::Deserialize)]
struct MetaFile {
	name: String,
	/// Seed as exact bit pattern (hex) — JSON float round-trips are not
	/// worth trusting with the terrain's identity.
	seed_bits: String,
	created_ms: u64,
	last_played_ms: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
#[serde(default)]
struct StateFile {
	inventory: Vec<(u16, u32)>,
	/// Hotbar slots, core wire encoding; empty in pre-arrangement saves
	/// (`serde(default)` keeps them parsing).
	hotbar: Vec<u16>,
	/// Inventory-grid slots, same encoding; empty in older saves.
	grid: Vec<u16>,
	/// Per-skill XP totals; empty in pre-skills saves (level 1).
	skills: Vec<(u8, u64)>,
	player: Option<([f32; 3], f32, f32)>,
}

struct Active {
	id: String,
	journal: Journal,
	dir: PathBuf,
	/// Latest snapshot not yet on disk.
	pending_state: Option<StateFile>,
	last_state_write: Instant,
}

pub struct FsWorldStore {
	root: PathBuf,
	active: Option<Active>,
	id_counter: u64,
}

impl FsWorldStore {
	pub fn new(root: PathBuf) -> Self {
		Self {
			root,
			active: None,
			id_counter: 0,
		}
	}

	/// Clock+counter id (no `rand` — the engine-seed precedent); the
	/// existence check makes even a frozen clock safe.
	fn fresh_id(&mut self) -> String {
		loop {
			let nanos = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.map_or(0, |d| d.as_nanos() as u64);
			self.id_counter += 1;
			let id = format!("{:016x}", nanos ^ (self.id_counter << 48));
			if !self.root.join(&id).exists() {
				return id;
			}
		}
	}

	fn write_state_now(active: &mut Active) {
		let Some(state) = active.pending_state.take() else {
			return;
		};
		let path = active.dir.join("state.json");
		let tmp = active.dir.join("state.json.tmp");
		let json = match serde_json::to_string(&state) {
			Ok(j) => j,
			Err(e) => {
				log::warn!("world state serialize failed: {e}");
				return;
			}
		};
		let write = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path));
		if let Err(e) = write {
			log::warn!("world state write failed ({e}) — keeping in memory");
			// Keep the snapshot so a later write retries it.
			active.pending_state = Some(state);
		}
		active.last_state_write = Instant::now();
	}

	fn read_meta(dir: &Path) -> Option<(MetaFile, f64)> {
		let text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
		let meta: MetaFile = serde_json::from_str(&text).ok()?;
		let bits = u64::from_str_radix(&meta.seed_bits, 16).ok()?;
		Some((meta, f64::from_bits(bits)))
	}

	fn write_meta(dir: &Path, meta: &MetaFile) -> std::io::Result<()> {
		let json =
			serde_json::to_string_pretty(meta).map_err(|e| std::io::Error::other(e.to_string()))?;
		std::fs::write(dir.join("meta.json"), json)
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |d| d.as_millis() as u64)
}

impl WorldStore for FsWorldStore {
	fn list_worlds(&mut self) -> Vec<WorldMeta> {
		let Ok(entries) = std::fs::read_dir(&self.root) else {
			return Vec::new();
		};
		let mut out = Vec::new();
		for entry in entries.flatten() {
			let dir = entry.path();
			if !dir.is_dir() {
				continue;
			}
			let Some((meta, seed)) = Self::read_meta(&dir) else {
				continue;
			};
			// A missing journal (created, never entered) is compatible;
			// a version-mismatched one is listed but refused for play.
			let compatible = matches!(world_version(&dir, seed), Ok(None));
			out.push(WorldMeta {
				id: entry.file_name().to_string_lossy().into_owned(),
				name: meta.name,
				seed,
				created_ms: meta.created_ms,
				last_played_ms: meta.last_played_ms,
				compatible,
			});
		}
		// Most recently played first.
		out.sort_by(|a, b| b.last_played_ms.cmp(&a.last_played_ms));
		out
	}

	fn create_world(&mut self, name: &str, seed: f64) -> Result<WorldMeta, String> {
		let id = self.fresh_id();
		let dir = self.root.join(&id);
		std::fs::create_dir_all(&dir).map_err(|e| format!("create failed: {e}"))?;
		let now = now_ms();
		let meta = MetaFile {
			name: name.to_string(),
			seed_bits: format!("{:016x}", seed.to_bits()),
			created_ms: now,
			last_played_ms: now,
		};
		Self::write_meta(&dir, &meta).map_err(|e| format!("create failed: {e}"))?;
		Ok(WorldMeta {
			id,
			name: meta.name,
			seed,
			created_ms: now,
			last_played_ms: now,
			compatible: true,
		})
	}

	fn delete_world(&mut self, id: &str) {
		if self.active.as_ref().is_some_and(|a| a.id == id) {
			// Deleting the open world: drop without a farewell flush.
			self.active = None;
		}
		if let Err(e) = std::fs::remove_dir_all(self.root.join(id)) {
			log::warn!("delete world {id}: {e}");
		}
	}

	fn open_world(&mut self, id: &str) -> Result<WorldSave, String> {
		self.close_world();
		let dir = self.root.join(id);
		let Some((mut meta, seed)) = Self::read_meta(&dir) else {
			return Err("save metadata unreadable".into());
		};
		// Refuse incompatible saves BEFORE Journal::open, whose archive-
		// and-start-fresh policy is right for a server volume and wrong
		// for a named user save. (A pre-shard save is NOT incompatible —
		// `Journal::open` migrates it in place.)
		match world_version(&dir, seed) {
			Ok(None) => {}
			Ok(Some(v)) => return Err(format!("save is from an old version (v{v})")),
			Err(e) => return Err(format!("save unreadable: {e}")),
		}
		let journal = Journal::open(&dir, seed).map_err(|e| format!("save unreadable: {e}"))?;
		let edits = journal.index.iter_edits().collect();
		let state: StateFile = std::fs::read_to_string(dir.join("state.json"))
			.ok()
			.and_then(|text| serde_json::from_str(&text).ok())
			.unwrap_or_default();
		meta.last_played_ms = now_ms();
		if let Err(e) = Self::write_meta(&dir, &meta) {
			log::warn!("meta stamp failed: {e}");
		}
		let save = WorldSave {
			meta: WorldMeta {
				id: id.to_string(),
				name: meta.name.clone(),
				seed,
				created_ms: meta.created_ms,
				last_played_ms: meta.last_played_ms,
				compatible: true,
			},
			edits,
			inventory: state.inventory.clone(),
			hotbar: state.hotbar.clone(),
			grid: state.grid.clone(),
			skills: state.skills.clone(),
			player: state
				.player
				.map(|(pos, yaw, pitch)| PlayerPersist { pos, yaw, pitch }),
		};
		self.active = Some(Active {
			id: id.to_string(),
			journal,
			dir,
			pending_state: None,
			last_state_write: Instant::now(),
		});
		Ok(save)
	}

	fn append_edit(&mut self, x: i32, y: i32, z: i32, cell: u16) {
		if let Some(active) = &mut self.active {
			active.journal.append(
				BlockEdit {
					x,
					y,
					z,
					block: cell,
				},
				SERVER_ID,
			);
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
				inventory: inventory.to_vec(),
				hotbar: hotbar.to_vec(),
				grid: grid.to_vec(),
				skills: skills.to_vec(),
				player: Some((player.pos, player.yaw, player.pitch)),
			});
		}
	}

	fn maintain(&mut self, _live_edits: Option<&EditOverlay>) {
		if let Some(active) = &mut self.active {
			active.journal.maintain();
			if active.pending_state.is_some()
				&& active.last_state_write.elapsed() >= Duration::from_secs(STATE_WRITE_SECS)
			{
				Self::write_state_now(active);
			}
		}
	}

	fn flush(&mut self, _live_edits: Option<&EditOverlay>) {
		if let Some(active) = &mut self.active {
			active.journal.sync();
			Self::write_state_now(active);
		}
	}

	fn close_world(&mut self) {
		self.flush(None);
		self.active = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn store(tag: &str) -> (FsWorldStore, PathBuf) {
		let root = std::env::temp_dir().join(format!("dbw-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&root);
		(FsWorldStore::new(root.clone()), root)
	}

	#[test]
	fn create_list_open_delete_roundtrip() {
		let (mut s, root) = store("crud");
		assert!(s.list_worlds().is_empty());
		let meta = s.create_world("Home", 42.0).unwrap();
		let list = s.list_worlds();
		assert_eq!(list.len(), 1);
		assert_eq!(list[0].name, "Home");
		assert_eq!(list[0].seed, 42.0);
		assert!(list[0].compatible, "journal-less save is compatible");

		// Enter, edit, save player state, close.
		let save = s.open_world(&meta.id).unwrap();
		assert!(save.edits.is_empty());
		assert!(save.player.is_none());
		s.append_edit(5, 130, -2, 0);
		s.append_edit(5, 130, -2, 4); // LWW
		s.append_edit(20, 131, 5, 3);
		s.save_player(
			&[(3, 12)],
			&[
				3,
				u16::MAX,
				u16::MAX - 1,
				u16::MAX,
				u16::MAX,
				u16::MAX,
				u16::MAX,
				u16::MAX,
				u16::MAX,
			],
			&[u16::MAX, 7, u16::MAX],
			&[(0, 1_068)],
			&PlayerPersist {
				pos: [1.0, 140.0, 2.0],
				yaw: 90.0,
				pitch: -10.0,
			},
		);
		s.close_world();

		// Reopen: edits (LWW), inventory, hotbar arrangement, and player
		// state all restore.
		let save = s.open_world(&meta.id).unwrap();
		let mut edits = save.edits.clone();
		edits.sort();
		assert_eq!(edits, vec![((5, 130, -2), 4), ((20, 131, 5), 3)]);
		assert_eq!(save.inventory, vec![(3, 12)]);
		assert_eq!(save.hotbar[0], 3);
		assert_eq!(save.hotbar[2], u16::MAX - 1, "custom tool slot kept");
		assert_eq!(save.grid[1], 7, "grid arrangement kept");
		assert_eq!(save.skills, vec![(0, 1_068)], "skill totals kept");
		let p = save.player.unwrap();
		assert_eq!(p.pos, [1.0, 140.0, 2.0]);
		assert_eq!((p.yaw, p.pitch), (90.0, -10.0));
		s.close_world();

		s.delete_world(&meta.id);
		assert!(s.list_worlds().is_empty());
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn two_worlds_are_distinct() {
		let (mut s, root) = store("two");
		let a = s.create_world("A", 1.0).unwrap();
		let b = s.create_world("B", 2.0).unwrap();
		assert_ne!(a.id, b.id);
		s.open_world(&a.id).unwrap();
		s.append_edit(1, 100, 1, 7);
		// Opening B closes (and flushes) A.
		let save_b = s.open_world(&b.id).unwrap();
		assert!(save_b.edits.is_empty());
		let save_a = s.open_world(&a.id).unwrap();
		assert_eq!(save_a.edits, vec![((1, 100, 1), 7)]);
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn incompatible_save_listed_but_refused() {
		let (mut s, root) = store("oldver");
		let meta = s.create_world("Old", 3.0).unwrap();
		// Enter once so the journal exists, then rewrite the header
		// version of the region shard the edit landed in.
		s.open_world(&meta.id).unwrap();
		s.append_edit(0, 100, 0, 1);
		s.close_world();
		let path = root
			.join(&meta.id)
			.join(format!("world-{:016x}", 3.0f64.to_bits()))
			.join("r.0.0.journal");
		let mut bytes = std::fs::read(&path).unwrap();
		bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
		std::fs::write(&path, &bytes).unwrap();

		let list = s.list_worlds();
		assert_eq!(list.len(), 1);
		assert!(!list[0].compatible, "old version flagged");
		let err = s.open_world(&meta.id).unwrap_err();
		assert!(err.contains("old version"), "{err}");
		// Crucially: the journal was NOT archived/wiped by the refusal.
		assert_eq!(std::fs::read(&path).unwrap(), bytes, "save untouched");
		let _ = std::fs::remove_dir_all(&root);
	}

	/// A pre-shard save is a CONTAINER mismatch, not a version one: it
	/// must list as compatible and open (migrating), never be refused
	/// alongside the genuinely stale ones.
	#[test]
	fn legacy_single_file_save_migrates_on_open() {
		let (mut s, root) = store("legacyshard");
		let meta = s.create_world("Old", 3.0).unwrap();
		let dir = root.join(&meta.id);
		// Hand-write the pre-shard layout: one whole-world journal file.
		let mut index = crate::journal::JournalIndex::default();
		for cell in [(1, 100, 1, 7u16), (900, 101, 900, 8)] {
			index.record(BlockEdit {
				x: cell.0,
				y: cell.1,
				z: cell.2,
				block: cell.3,
			});
		}
		let legacy = dir.join(format!("world-{:016x}.journal", 3.0f64.to_bits()));
		std::fs::write(&legacy, crate::journal::snapshot_bytes(3.0, &index)).unwrap();

		assert!(
			s.list_worlds()[0].compatible,
			"old container, not old format"
		);
		let save = s.open_world(&meta.id).unwrap();
		let mut edits = save.edits.clone();
		edits.sort();
		assert_eq!(edits, vec![((1, 100, 1), 7), ((900, 101, 900), 8)]);
		s.close_world();
		assert!(!legacy.exists(), "migrated, not left behind");
		assert!(dir
			.join(format!("world-{:016x}", 3.0f64.to_bits()))
			.join("r.0.0.journal")
			.exists());
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn corrupt_state_json_degrades_to_empty() {
		let (mut s, root) = store("badstate");
		let meta = s.create_world("W", 5.0).unwrap();
		s.open_world(&meta.id).unwrap();
		s.append_edit(0, 100, 0, 1);
		s.close_world();
		std::fs::write(root.join(&meta.id).join("state.json"), b"{nope").unwrap();
		let save = s.open_world(&meta.id).unwrap();
		assert_eq!(save.edits.len(), 1, "journal intact");
		assert!(save.inventory.is_empty());
		assert!(save.player.is_none());
		let _ = std::fs::remove_dir_all(&root);
	}

	/// A pre-arrangement state.json (no `hotbar` field) parses and
	/// surfaces an empty arrangement — the engine's auto-assign fallback.
	#[test]
	fn legacy_state_json_without_hotbar_parses() {
		let (mut s, root) = store("legacy");
		let meta = s.create_world("W", 6.0).unwrap();
		s.open_world(&meta.id).unwrap();
		s.append_edit(0, 100, 0, 1);
		s.close_world();
		std::fs::write(
			root.join(&meta.id).join("state.json"),
			br#"{"inventory":[[3,7]],"player":[[1.0,140.0,2.0],0.0,0.0]}"#,
		)
		.unwrap();
		let save = s.open_world(&meta.id).unwrap();
		assert_eq!(save.inventory, vec![(3, 7)]);
		assert!(save.hotbar.is_empty(), "legacy save: no arrangement");
		assert!(save.player.is_some());
		let _ = std::fs::remove_dir_all(&root);
	}
}
