use super::*;
use digblock_core::worldgen::Seed;
use std::cell::RefCell;
use std::rc::Rc;

/// Records WorldStore calls; open returns a save with one edit and a
/// player position.
#[derive(Default)]
struct Calls {
	appended: Vec<(i32, i32, i32, u16)>,
	saved_player: u32,
	saved_hotbar: Vec<u16>,
	/// Arrangement `open_world` hands back (empty = legacy save).
	open_hotbar: Vec<u16>,
	maintained: u32,
	flushed: u32,
	closed: u32,
}

struct MockStore(Rc<RefCell<Calls>>);

impl digblock_persist::WorldStore for MockStore {
	fn list_worlds(&mut self) -> Vec<digblock_persist::WorldMeta> {
		Vec::new()
	}
	fn create_world(
		&mut self,
		name: &str,
		seed: Seed,
	) -> Result<digblock_persist::WorldMeta, String> {
		Ok(digblock_persist::WorldMeta {
			id: "w1".into(),
			name: name.into(),
			seed,
			created_ms: 0,
			last_played_ms: 0,
			compatible: true,
		})
	}
	fn delete_world(&mut self, _id: &str) {}
	fn open_world(&mut self, id: &str) -> Result<digblock_persist::WorldSave, String> {
		Ok(digblock_persist::WorldSave {
			meta: digblock_persist::WorldMeta {
				id: id.into(),
				name: "W".into(),
				seed: Seed::current(7.0),
				created_ms: 0,
				last_played_ms: 0,
				compatible: true,
			},
			// A LEGACY save on purpose (rev 0): several tests here feed
			// layouts written when the bucket was owned by everyone, and
			// the migration grant is part of what they exercise — as is
			// §14's essence reset, which the flagged pairs below are for.
			rev: 0,
			edits: vec![((1, 100, 1), 0)],
			inventory: vec![
				(3, 5),
				(digblock_core::inventory::SUB_WIRE_FLAG, 500),
				(digblock_core::inventory::SUB_WIRE_FLAG | 4, 7),
			],
			hotbar: self.0.borrow().open_hotbar.clone(),
			grid: Vec::new(),
			skills: Vec::new(),
			player: Some(digblock_persist::PlayerPersist {
				pos: [1.5, 140.0, 2.5],
				yaw: 90.0,
				pitch: -10.0,
			}),
		})
	}
	fn append_edit(&mut self, x: i32, y: i32, z: i32, cell: u16) {
		self.0.borrow_mut().appended.push((x, y, z, cell));
	}
	fn save_player(
		&mut self,
		_inventory: &[(u16, u32)],
		hotbar: &[u16],
		_grid: &[u16],
		_skills: &[(u8, u64)],
		_player: &digblock_persist::PlayerPersist,
	) {
		let mut state = self.0.borrow_mut();
		state.saved_player += 1;
		state.saved_hotbar = hotbar.to_vec();
	}
	fn maintain(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {
		self.0.borrow_mut().maintained += 1;
	}
	fn flush(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {
		self.0.borrow_mut().flushed += 1;
	}
	fn close_world(&mut self) {
		self.0.borrow_mut().closed += 1;
	}
}

#[test]
fn create_enter_edits_and_snapshots_flow_to_store() {
	let calls = Rc::new(RefCell::new(Calls::default()));
	let mut config = EngineConfig::desktop();
	config.menu = true;
	config.workers = 0; // deterministic inline executor
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	engine.set_world_store(Box::new(MockStore(calls.clone())));

	engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
	assert!(engine.save.in_local_save, "entered the created world");
	assert_eq!(engine.seed, Seed::current(7.0), "world seed from the save");
	assert_eq!(
		engine.player.pos.to_array(),
		[1.5, 140.0, 2.5],
		"restored position"
	);
	assert!(engine.pipeline.overlay().is_some(), "overlay installed");

	// Headless: pump_pipeline (render path) never runs, so generate
	// the world directly until the edit's chunk is Ready.
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = engine.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}

	// A local edit, then enough updates to cross the snapshot interval:
	// the edit reaches the store and a player snapshot fires; maintain
	// runs every frame.
	let new = if engine.pipeline.block_at(1, 101, 1) == Some(BlockType::Stone) {
		BlockType::Dirt
	} else {
		BlockType::Stone
	};
	engine
		.pipeline
		.set_block(1, 101, 1, new)
		.expect("chunk ready for the test edit");
	for _ in 0..400 {
		engine.update(Duration::from_millis(16));
	}
	let c = calls.borrow();
	assert!(c.maintained > 300, "store maintained per frame");
	assert!(c.saved_player >= 1, "player snapshot fired after 5s");
	assert!(
		c.appended.contains(&(1, 101, 1, new as u16)),
		"edit forwarded: {:?}",
		c.appended
	);
	drop(c);

	// Suspend: final snapshot + flush, synchronously.
	engine.on_suspend();
	let c = calls.borrow();
	assert!(c.flushed >= 1, "suspend flushes");
	assert!(c.saved_player >= 2, "suspend snapshots the player");
}

/// The two save-rev migrations, off one rev-0 fixture: the bucket
/// grant (rev 1) and §14's essence reset (rev 2) — every balance but
/// Aether clears, once, on load. The server's record reader runs the
/// same reset at its own version, so the two careers cross the
/// redesign identically.
#[test]
fn a_legacy_save_grants_the_bucket_and_resets_pre_redesign_essence() {
	use digblock_core::block::Essence;
	let calls = Rc::new(RefCell::new(Calls::default()));
	let mut config = EngineConfig::desktop();
	config.menu = true;
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	engine.set_world_store(Box::new(MockStore(calls.clone())));
	engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
	assert_eq!(
		engine.inventory.count(BlockType::Bucket),
		1,
		"a rev-0 save keeps its implicit bucket"
	);
	assert_eq!(
		engine.inventory.sub_count(Essence::Vitae),
		0,
		"pre-redesign balances clear"
	);
	assert_eq!(
		engine.inventory.sub_count(Essence::Aether),
		7,
		"Aether alone survives the reset"
	);
	assert_eq!(engine.inventory.count(BlockType::Grass), 5, "stacks stay");
}

#[test]
fn saved_hotbar_arrangement_restores_and_round_trips() {
	use digblock_core::hotbar::{HOTBAR_SLOTS, HotbarItem, WIRE_EMPTY, WIRE_TOOL};

	let calls = Rc::new(RefCell::new(Calls::default()));
	// Custom arrangement: the tool parked at the head of the bar, and
	// the inventory's block partway along it.
	let mut wire = [WIRE_EMPTY; HOTBAR_SLOTS];
	wire[0] = WIRE_TOOL;
	wire[4] = 3;
	calls.borrow_mut().open_hotbar = wire.to_vec();

	let mut config = EngineConfig::desktop();
	config.menu = true;
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	engine.set_world_store(Box::new(MockStore(calls.clone())));
	engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });

	// §6 pinned the tool slot: a legacy layout that parked the tool
	// mid-bar migrates on decode — the parked spot empties and the
	// tool comes back at the end; blocks keep their slots.
	let b3 = BlockType::try_from_id(3).unwrap();
	assert_eq!(engine.hotbar.slots()[0], None, "parked spot empties");
	assert_eq!(engine.hotbar.slots()[4], Some(HotbarItem::Block(b3)));
	assert_eq!(
		engine.hotbar.slots()[HOTBAR_SLOTS - 1],
		Some(HotbarItem::WaterTool),
		"the tool slot is pinned to the end"
	);

	// Suspend hands the MIGRATED arrangement back to the store.
	engine.on_suspend();
	let mut migrated = [WIRE_EMPTY; HOTBAR_SLOTS];
	migrated[4] = 3;
	migrated[HOTBAR_SLOTS - 1] = WIRE_TOOL;
	assert_eq!(calls.borrow().saved_hotbar, migrated.to_vec());
}

/// The walk sim holds while the player's column is ungenerated —
/// unloaded cells are non-solid, so ticking would free-fall the
/// player through the world (the web underground-spawn bug).
#[test]
fn walk_sim_holds_until_player_column_generates() {
	let calls = Rc::new(RefCell::new(Calls::default()));
	let mut config = EngineConfig::desktop();
	config.menu = true;
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	engine.set_world_store(Box::new(MockStore(calls)));
	// MockStore's save carries a different seed from the backdrop, so the
	// pipeline is rebuilt from scratch and nothing is generated at entry.
	engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
	let start = engine.player.pos;

	// Headless (no render pump): the world never generates — the
	// player must hold at the restored position instead of falling.
	for _ in 0..60 {
		engine.update(Duration::from_millis(16));
	}
	assert_eq!(engine.player.pos, start, "held over ungenerated world");

	// Once the world lands, the sim resumes (gravity takes over).
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = engine.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
	for _ in 0..60 {
		engine.update(Duration::from_millis(16));
	}
	assert!(engine.player.pos.y < start.y, "sim resumed after gen");
}

/// Echoes `create_world`'s seed back through `open_world` with a
/// pristine save, the way real stores do — Create must then keep the
/// backdrop pipeline (same seed, no edits) instead of regenerating.
struct EchoStore(Seed);

impl digblock_persist::WorldStore for EchoStore {
	fn list_worlds(&mut self) -> Vec<digblock_persist::WorldMeta> {
		Vec::new()
	}
	fn create_world(
		&mut self,
		name: &str,
		seed: Seed,
	) -> Result<digblock_persist::WorldMeta, String> {
		self.0 = seed;
		Ok(digblock_persist::WorldMeta {
			id: "w1".into(),
			name: name.into(),
			seed,
			created_ms: 0,
			last_played_ms: 0,
			compatible: true,
		})
	}
	fn delete_world(&mut self, _id: &str) {}
	fn open_world(&mut self, id: &str) -> Result<digblock_persist::WorldSave, String> {
		Ok(digblock_persist::WorldSave {
			meta: digblock_persist::WorldMeta {
				id: id.into(),
				name: "W".into(),
				seed: self.0,
				created_ms: 0,
				last_played_ms: 0,
				compatible: true,
			},
			rev: digblock_persist::SAVE_REV,
			edits: Vec::new(),
			inventory: Vec::new(),
			hotbar: Vec::new(),
			grid: Vec::new(),
			skills: Vec::new(),
			player: None,
		})
	}
	fn append_edit(&mut self, _x: i32, _y: i32, _z: i32, _cell: u16) {}
	fn save_player(
		&mut self,
		_inventory: &[(u16, u32)],
		_hotbar: &[u16],
		_grid: &[u16],
		_skills: &[(u8, u64)],
		_player: &digblock_persist::PlayerPersist,
	) {
	}
	fn maintain(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {}
	fn flush(&mut self, _live: Option<&digblock_core::overlay::EditOverlay>) {}
	fn close_world(&mut self) {}
}

#[test]
fn create_reuses_streamed_backdrop_world() {
	let mut config = EngineConfig::desktop();
	config.menu = true;
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);
	engine.set_world_store(Box::new(EchoStore(Seed::current(0.0))));

	// Stream the backdrop in around its spawn, like the menu does.
	let spawn = digblock_core::player::spawn(Seed::current(3.0));
	let center = digblock_core::chunk::ChunkPos::new(
		spawn.x.floor() as i32 / 16,
		0,
		spawn.z.floor() as i32 / 16,
	);
	for _ in 0..10_000 {
		let m = engine.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
	let ground = (
		spawn.x.floor() as i32,
		(spawn.y - digblock_core::player::EYE_TO_FEET - 0.5).floor() as i32,
		spawn.z.floor() as i32,
	);
	assert!(
		engine
			.pipeline
			.cell_at(ground.0, ground.1, ground.2)
			.is_some()
	);

	engine.handle_world_intent(crate::menu::WorldIntent::Create { name: "W".into() });
	assert!(engine.save.in_local_save);
	assert_eq!(
		engine.seed,
		Seed::current(3.0),
		"create adopted the backdrop seed"
	);
	// Retained pipeline: the spawn column is queryable with no
	// further maintain — the player stands instead of waiting.
	assert!(
		engine
			.pipeline
			.cell_at(ground.0, ground.1, ground.2)
			.is_some(),
		"backdrop chunks survived world entry"
	);
	for _ in 0..60 {
		engine.update(Duration::from_millis(16));
	}
	assert!(
		(engine.player.pos.y - spawn.y).abs() < 2.0,
		"player settled at spawn, not underground (y {} vs spawn {})",
		engine.player.pos.y,
		spawn.y
	);
}
