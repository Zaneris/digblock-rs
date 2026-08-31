//! The cast channel's contract: a channel takes exactly the ladder's
//! wind-up, an interrupt resets it outright, and the teleport cooldown
//! the completion starts is the caster's own rung and covers EVERY
//! teleport — every expectation DERIVED from `unlock`, never a tick
//! count written down.

use super::*;
use digblock_core::skills::Skill;
use digblock_platform::{InputEvent, Key};

fn fresh() -> Engine {
	let mut config = EngineConfig::desktop();
	config.interactive = true;
	config.workers = 0;
	config.seed = Some(3.0);
	Engine::new(config)
}

/// Ask for the home channel the way the spellbook's Home row does. The
/// request lands ahead of the cast step within one `tick`, exactly as the
/// drained key request did, so every count below is in channel ticks.
fn cast_home(e: &mut Engine) {
	e.start_cast(CastTarget::Home);
}

/// Stream the world about spawn in until nothing is in flight — a cast
/// that writes cells needs every cell of its sphere resident, and a
/// head cell that is not loaded hosts nothing.
fn settle(e: &mut Engine) {
	let center = digblock_core::chunk::ChunkPos::new(0, 0, 0);
	for _ in 0..10_000 {
		let m = e.pipeline.maintain(center, usize::MAX, None, None);
		if m.gen_queue == 0 && m.mesh_queue == 0 && m.in_flight == 0 && !m.saturated {
			break;
		}
	}
}

/// One engine step of exactly one sim tick, so the fixed-rate cast
/// stepping advances once per call.
fn tick(e: &mut Engine) {
	e.step(std::time::Duration::from_secs_f32(
		digblock_core::player::TICK,
	));
}

/// What the sweeps build a sphere of: crafted brick, which no terrain
/// holds — so a shell cell counted as built was built — and no sweep
/// turns over, unlike the dirt the spell admits first, which the grass
/// job greens mid-climb (a cell no longer holding the block is rightly
/// left standing by an unwind, and stays paid for).
fn matter() -> digblock_core::block::BlockType {
	let b = digblock_core::block::BlockType::StoneBrick;
	assert!(digblock_core::sphere::accepts(b), "brick builds");
	b
}

/// The two fares one sphere drains at `level` — its own and the wisp
/// its seal lights — read off the tables.
fn sphere_fares(level: u32) -> [(digblock_core::block::Essence, u32); 2] {
	[
		digblock_core::alchemy::sphere_price(level),
		digblock_core::alchemy::wisp_price(level),
	]
}

/// A caster's balance in each fare, in table order.
fn balances(e: &Engine, level: u32) -> Vec<u32> {
	sphere_fares(level)
		.iter()
		.map(|&(essence, _)| e.inventory.sub_count(essence))
		.collect()
}

/// Put `n` of `block` in the caster's hand: the count in the inventory,
/// the type on the first bar slot, and the bar pointing at it.
fn hold(e: &mut Engine, block: digblock_core::block::BlockType, n: u32) {
	e.inventory.add(block, n);
	assert!(e.hotbar.assign(0, block), "the first slot takes the block");
	e.hotbar.set_selected(0);
}

/// The shell the engine would bill this caster for right now: the plan
/// at the body's head, less the cells protection would keep it from
/// writing — the same subtraction the offline arm makes before it bills.
fn shell_bill(e: &Engine, r: i32, block: digblock_core::block::BlockType) -> u32 {
	let head = digblock_core::kindle::host_cell(e.body_pos());
	let mut writes =
		digblock_core::sphere::plan(head, r, block, |c| e.pipeline.cell_at(c.0, c.1, c.2))
			.expect("the sphere's cells are resident");
	writes.retain(|&(c, _)| !digblock_core::protect::immutable(c.0, c.1, c.2, &e.protected));
	digblock_core::sphere::blocks_needed(&writes, block)
}

/// Arm a caster for exactly one sphere at the first rank: the rung, both
/// fares, and just the blocks the shell about them bills, in hand. The
/// world is settled first, since the bill is read off the plan. Answers
/// the level and the count billed.
fn arm_sphere(e: &mut Engine) -> (u32, u32) {
	settle(e);
	let level = digblock_core::unlock::SPHERE_AT;
	e.skills
		.add_xp(Skill::Arcana, digblock_core::skills::total_xp_for(level));
	for (essence, fare) in sphere_fares(level) {
		e.inventory.add_sub(essence, fare);
	}
	let need = shell_bill(e, digblock_core::unlock::sphere_radius(level), matter());
	assert!(need > 10, "a sphere about spawn is worth billing");
	hold(e, matter(), need);
	(level, need)
}

/// Whether the feed has spoken this line.
fn spoke(e: &Engine, msg: &Msg) -> bool {
	e.session.feed.iter().any(|(m, _, _)| m == msg)
}

#[test]
fn a_completed_channel_takes_channel_ticks_and_starts_the_rung_cooldown() {
	let mut e = fresh();
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	cast_home(&mut e);
	tick(&mut e); // drains the request into a live channel
	assert!(e.cast.is_some(), "H did not open a channel");
	// One tick shy: still winding up, nothing fired.
	for _ in 0..need - 2 {
		tick(&mut e);
	}
	assert!(e.cast.is_some(), "the channel completed early");
	assert_eq!(e.save.teleport_cooldown.left(), 0.0);
	tick(&mut e);
	assert!(e.cast.is_none(), "the channel never completed");
	// The completion started the caster's OWN cooldown rung.
	let want = digblock_core::unlock::teleport_cooldown_secs(e.skills.level(Skill::Arcana));
	assert_eq!(e.save.teleport_cooldown.left().ceil() as u32, want);
	// And a fresh request during the cooldown opens the PAGE (where the
	// countdown lives) instead of silently doing nothing.
	cast_home(&mut e);
	tick(&mut e);
	assert!(e.cast.is_none());
	assert!(
		e.ui.spellbook.is_open(),
		"a cooling cast must show the page"
	);
}

/// The teleport's light is engine state on confetti's rules: a completed
/// jump throws its two bursts, and both expire within the shader's own
/// LIFE — an effect the shader has faded to zero must not stay uploaded.
#[test]
fn a_jump_throws_bursts_that_expire_within_their_life() {
	let mut e = fresh();
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	cast_home(&mut e);
	for _ in 0..=need {
		tick(&mut e);
	}
	assert!(e.cast.is_none(), "the channel should have completed");
	assert_eq!(e.vfx.casts.len(), 2, "a jump is a departure AND an arrival");
	assert!(e.vfx.flash > 0.0, "the jump flashes");
	let ticks = (crate::teleport::LIFE / digblock_core::player::TICK) as u32 + 2;
	for _ in 0..ticks {
		tick(&mut e);
	}
	assert!(e.vfx.casts.is_empty(), "bursts outlived the shader's fade");
	assert_eq!(e.vfx.flash, 0.0, "the flash outlived the jump");
}

/// The Sphere climbs THROUGH its channel and a break unwinds it: halfway
/// through, part of the shell stands and the whole bill is gone — both
/// fares and the held blocks; one step puts every block back and the
/// bill with it; a channel left to finish seals the whole shell, washes
/// the overlay, keeps the bill, lights the wisp and pays both fares' XP.
#[test]
fn a_broken_sphere_channel_unwinds_and_a_finished_one_seals() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::{cell_id, make_cell};
	use digblock_core::skills::{cast_xp, total_xp_for};
	let mut e = fresh();
	let (level, billed) = arm_sphere(&mut e);
	let r = digblock_core::unlock::sphere_radius(level);
	let wall = make_cell(matter(), 0);
	// The shell's writable air about wherever the caster stands now —
	// read again after the step below, since the head moves with them.
	let shell_air = |e: &Engine| -> Vec<(i32, i32, i32)> {
		let head = digblock_core::kindle::host_cell(e.body_pos());
		let (shell, _) = digblock_core::sphere::sets(head, r);
		shell
			.into_iter()
			.filter(|&(x, y, z)| {
				e.pipeline.cell_at(x, y, z).map(cell_id) == Some(BlockType::Air as u16)
					&& !digblock_core::protect::immutable(x, y, z, &e.protected)
			})
			.collect()
	};
	let built = |e: &Engine, cells: &[(i32, i32, i32)]| {
		cells
			.iter()
			.filter(|&&(x, y, z)| e.pipeline.cell_at(x, y, z) == Some(wall))
			.count()
	};
	let air_cells = shell_air(&e);
	assert!(
		air_cells.len() > 10,
		"the shell about spawn must be mostly air, or the sweep proves nothing"
	);
	assert_eq!(
		air_cells.len() as u32,
		billed,
		"the bill is the shell's writable air"
	);
	let need = digblock_core::unlock::channel_ticks(level);

	e.start_cast(CastTarget::Sphere);
	assert!(e.cast.is_some(), "the channel opened");
	assert_eq!(
		balances(&e, level),
		vec![0, 0],
		"both fares are taken as the channel begins"
	);
	assert_eq!(e.inventory.count(matter()), 0, "and the blocks with them");
	for _ in 0..need / 2 {
		tick(&mut e);
	}
	let half = built(&e, &air_cells);
	assert!(
		half > 0 && half < air_cells.len(),
		"halfway through, the shell is climbing"
	);
	e.input.dig_held = true;
	tick(&mut e);
	assert!(e.cast.is_none(), "the dig hold broke the channel");
	assert_eq!(built(&e, &air_cells), 0, "every block came back down");
	assert_eq!(
		balances(&e, level),
		sphere_fares(level).iter().map(|f| f.1).collect::<Vec<_>>(),
		"and the fares came back"
	);
	assert_eq!(e.inventory.count(matter()), billed, "and the blocks");
	assert_eq!(e.wisp_left, 0.0, "a broken channel lights nothing");
	e.input.dig_held = false;

	// The refund landed the type wherever the layout had room — a slot
	// whose count reached zero mid-climb was released — so the hand is
	// HELD again before the recast.
	let air_cells = shell_air(&e);
	let again = air_cells.len() as u32;
	hold(&mut e, matter(), again.saturating_sub(billed));
	let have = e.inventory.count(matter());
	e.start_cast(CastTarget::Sphere);
	assert!(e.cast.is_some(), "{:?}", e.session.feed);
	for _ in 0..=need {
		tick(&mut e);
	}
	assert!(e.cast.is_none(), "the channel completed");
	assert_eq!(
		built(&e, &air_cells),
		air_cells.len(),
		"the shell sealed whole"
	);
	assert!(e.vfx.flash > 0.0, "the seal washes the overlay");
	assert_eq!(balances(&e, level), vec![0, 0], "the fares stay spent");
	assert_eq!(
		e.inventory.count(matter()),
		have - again,
		"every block billed was placed, so none comes back"
	);
	assert!(e.wisp_left > 0.0, "the seal lit the wisp");
	let paid: u64 = sphere_fares(level)
		.iter()
		.map(|&(essence, fare)| cast_xp(essence, fare))
		.sum();
	assert_eq!(
		e.skills.xp(Skill::Arcana),
		total_xp_for(level) + paid,
		"the seal paid for the sphere and the wisp it lit"
	);
}

/// The hand is gated before anything is paid, and each refusal names
/// what is wrong: an empty hand, a block no shell can be built of, too
/// few of the right one — the last naming the count. Nothing leaves the
/// inventory for any of them and no channel opens.
#[test]
fn a_sphere_refuses_the_hand_it_cannot_build_from_and_says_why() {
	use digblock_core::block::BlockType;
	let mut e = fresh();
	let (level, billed) = arm_sphere(&mut e);
	let fares: Vec<u32> = sphere_fares(level).iter().map(|f| f.1).collect();
	let untouched = |e: &Engine| balances(e, level) == fares && e.cast.is_none();

	// Nothing on the bar under the selection.
	e.hotbar.set_selected(1);
	assert_eq!(e.hotbar.selected_item(), None, "slot 1 is bare");
	e.start_cast(CastTarget::Sphere);
	assert!(spoke(&e, &Msg::SphereNoBlock), "an empty hand");
	assert!(untouched(&e));

	// A door — two cells — under it.
	assert!(!digblock_core::sphere::accepts(BlockType::LumberDoor));
	e.inventory.add(BlockType::LumberDoor, 1);
	assert!(e.hotbar.assign(1, BlockType::LumberDoor));
	e.start_cast(CastTarget::Sphere);
	assert!(
		spoke(&e, &Msg::SphereBadBlock(Some(BlockType::LumberDoor))),
		"a door, named"
	);
	assert!(untouched(&e));
	assert_eq!(e.inventory.count(BlockType::LumberDoor), 1);

	// The right block, one short.
	e.hotbar.set_selected(0);
	assert!(e.inventory.try_take(matter(), 1));
	e.start_cast(CastTarget::Sphere);
	assert!(
		spoke(
			&e,
			&Msg::SphereBlocksShort(Some((matter(), billed, billed - 1)))
		),
		"one short, with the count: {:?}",
		e.session.feed
	);
	assert!(untouched(&e));
	assert_eq!(e.inventory.count(matter()), billed - 1);

	// And with the block back, the channel opens.
	e.inventory.add(matter(), 1);
	e.start_cast(CastTarget::Sphere);
	assert!(e.cast.is_some(), "the hand is good now");
}

/// An interrupted channel FADES rather than pops: the aura's envelope is
/// still visible the tick after the break, and then runs monotonically
/// to zero.
#[test]
fn an_interrupted_channel_fades_rather_than_pops() {
	let mut e = fresh();
	cast_home(&mut e);
	// Long enough for the envelope to open fully.
	let open = (crate::teleport::FADE_IN_SECS / digblock_core::player::TICK) as u32 + 2;
	for _ in 0..open {
		tick(&mut e);
	}
	assert_eq!(e.vfx.channel_env.level(), 1.0, "the aura never opened");
	e.input.dig_held = true;
	tick(&mut e);
	e.input.dig_held = false;
	assert!(e.cast.is_none(), "the interrupt did not land");
	let mut prev = e.vfx.channel_env.level();
	assert!(prev > 0.0, "the aura popped instead of fading");
	let mut steps = 0u32;
	while e.vfx.channel_env.level() > 0.0 {
		tick(&mut e);
		assert!(e.vfx.channel_env.level() <= prev, "the fade went back up");
		prev = e.vfx.channel_env.level();
		steps += 1;
		assert!(steps < 600, "the fade never reaches zero");
	}
	assert!(steps > 1, "the fade was a pop with extra steps");
}

#[test]
fn an_interrupt_resets_the_channel() {
	let mut e = fresh();
	cast_home(&mut e);
	tick(&mut e);
	assert!(e.cast.is_some());
	// Walking is NOT an interrupt: a caster may move while channelling.
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: true,
	});
	tick(&mut e);
	assert!(e.cast.is_some(), "a walking caster lost the channel");
	e.input(InputEvent::Key {
		key: Key::W,
		pressed: false,
	});
	// A dig hold is: one tick of it resets the channel outright.
	e.input.dig_held = true;
	tick(&mut e);
	assert!(e.cast.is_none(), "a digging caster kept the channel");
	assert_eq!(
		e.save.teleport_cooldown.left(),
		0.0,
		"an interrupted channel must not charge the cooldown"
	);
	e.input.dig_held = false;
	// A reset is a reset: the next channel starts from nothing and runs
	// its full length again.
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	cast_home(&mut e);
	tick(&mut e);
	for _ in 0..need - 2 {
		tick(&mut e);
	}
	assert!(e.cast.is_some(), "the second channel inherited progress");
}

/// One clock, every teleport: a port cast arms the same cooldown Return
/// Home does, and neither can start while it runs. Swept over both
/// directions, because a per-spell timer would leave exactly one of them
/// open and the bug is invisible from the other side.
#[test]
fn one_cooldown_covers_home_and_every_port() {
	// Both orders: the spell that arms the clock, then the spell that
	// must find it armed.
	for (first, second) in [
		(CastTarget::Home, CastTarget::Waystone(0)),
		(CastTarget::Waystone(0), CastTarget::Home),
	] {
		let mut e = fresh();
		let level = e.skills.level(Skill::Arcana);
		let need = digblock_core::unlock::channel_ticks(level);
		e.start_cast(first);
		for _ in 0..=need {
			tick(&mut e);
		}
		assert!(e.cast.is_none(), "{first:?} never completed");
		assert_eq!(
			e.save.teleport_cooldown.left().ceil() as u32,
			digblock_core::unlock::teleport_cooldown_secs(level),
			"{first:?} did not arm the shared clock"
		);
		e.start_cast(second);
		assert!(
			e.cast.is_none(),
			"{second:?} started inside {first:?}'s cooldown"
		);
		assert!(
			e.ui.spellbook.is_open(),
			"a cooling {second:?} must show the page"
		);
	}
}

/// The conjuring spells are NOT teleports and never touch the clock —
/// the predicate that decides is asked, not restated here.
#[test]
fn a_light_spell_neither_waits_on_the_clock_nor_arms_it() {
	let mut e = fresh();
	// The Sphere validates as its channel BEGINS, so it needs its rung,
	// its bill and a resident sphere to open at all — and the wisp cast
	// ahead of it in the sweep spends the wisp's fare, which the sphere's
	// seal drains again, so that fare is stocked twice.
	let (level, _) = arm_sphere(&mut e);
	let (wisp_essence, wisp_fare) = digblock_core::alchemy::wisp_price(level);
	e.inventory.add_sub(wisp_essence, wisp_fare);
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	for target in [CastTarget::Wisp, CastTarget::Kindle, CastTarget::Sphere] {
		assert!(!target.teleports(), "{target:?} must not ride the clock");
		// Mid-window: long enough that the channel cannot run it out, so
		// "still running" is what the cast happened UNDER.
		let armed = digblock_core::unlock::teleport_cooldown_secs(e.skills.level(Skill::Arcana));
		let before = armed as f32 / 2.0;
		e.save.teleport_cooldown.force(before);
		e.start_cast(target);
		assert!(e.cast.is_some(), "{target:?} was blocked by the clock");
		for _ in 0..=need {
			tick(&mut e);
		}
		let left = e.save.teleport_cooldown.left();
		assert!(left > 0.0, "the window ran out — the sweep proved nothing");
		assert!(
			left < before,
			"{target:?} left the clock standing instead of letting it run"
		);
	}
}

/// The offline kindle: the light stands in the cell the caster's head is
/// in, not where they aim; it burns for exactly `kindle::SECS` on its own
/// clock; and moving the caster — a port — leaves it where it was lit.
/// The span, the rung and the fare are all read off the tables.
#[test]
fn a_kindled_light_stands_at_the_head_on_its_own_clock() {
	use digblock_core::skills::total_xp_for;
	let mut e = fresh();
	// Settle the terrain, or the head cell is unloaded and hosts nothing.
	settle(&mut e);
	let level = digblock_core::unlock::KINDLE_AT;
	e.skills.add_xp(Skill::Arcana, total_xp_for(level));
	let (essence, fare) = digblock_core::alchemy::kindle_price(level);
	e.inventory.add_sub(essence, fare);
	// Look straight down: an aimed kindle would land on the floor cell.
	e.look.pitch_deg = -80.0;
	let head = digblock_core::kindle::host_cell(e.player.pos);
	assert!(
		digblock_core::kindle::hosts(e.pipeline.cell_at(head.0, head.1, head.2)),
		"the spawn head cell must host, or the sweep proves nothing"
	);
	e.start_cast(CastTarget::Kindle);
	let need = digblock_core::unlock::channel_ticks(level);
	for _ in 0..=need {
		tick(&mut e);
	}
	assert_eq!(e.vfx.kindled.len(), 1, "one light stands");
	assert_eq!(e.vfx.kindled[0].cell, head, "in the head's own cell");
	assert_eq!(e.inventory.sub_count(essence), 0, "the fare left");
	assert!(
		e.skills.xp(Skill::Arcana) > total_xp_for(level),
		"the cast paid its XP"
	);
	// A port: the light does not follow.
	e.player.pos += Vec3::new(40.0, 0.0, 40.0);
	e.step(std::time::Duration::from_secs_f32(
		digblock_core::kindle::SECS as f32 - 1.0,
	));
	assert_eq!(
		e.vfx.kindled.len(),
		1,
		"still burning a second short of its span"
	);
	assert_eq!(e.vfx.kindled[0].cell, head, "and still where it was lit");
	e.step(std::time::Duration::from_secs_f32(1.5));
	assert!(e.vfx.kindled.is_empty(), "and out once the span has run");
}

/// Fly leaves `player.pos` where the body last stood, and both conjuring
/// spells derive their cell from the head: cast while flying and the
/// sphere closes about a spot you left, with your light standing in it.
/// Both read `body_pos`, the same position the shell is scanned
/// against, so the answer is one position rather than two.
#[test]
fn a_flying_caster_conjures_where_they_are() {
	use digblock_core::skills::total_xp_for;
	let mut e = fresh();
	let (_, billed) = arm_sphere(&mut e);
	let level = digblock_core::unlock::SPHERE_AT.max(digblock_core::unlock::KINDLE_AT);
	e.skills.add_xp(Skill::Arcana, total_xp_for(level));
	let (essence, fare) = digblock_core::alchemy::kindle_price(level);
	e.inventory.add_sub(essence, fare);
	// Fly a few blocks up and along: the camera moves, the body does not.
	e.mode = crate::engine::CameraMode::DebugFly;
	let flown = e.player.pos + Vec3::new(3.0, 4.0, -2.0);
	e.camera.pos = flown;
	let stale = digblock_core::kindle::host_cell(e.player.pos);
	let here = digblock_core::kindle::host_cell(e.body_pos());
	assert_ne!(here, stale, "the fly must actually have gone somewhere");
	// The shell up here bills differently from the one on the ground
	// the fixture sized the hand for; top the hand up to whatever it asks.
	let r = digblock_core::unlock::sphere_radius(e.skills.level(Skill::Arcana));
	let up_here = shell_bill(&e, r, matter());
	e.inventory.add(matter(), up_here.saturating_sub(billed));

	let need = digblock_core::unlock::channel_ticks(level);
	e.start_cast(CastTarget::Kindle);
	for _ in 0..=need {
		tick(&mut e);
	}
	assert_eq!(e.vfx.kindled.len(), 1, "the light stood");
	assert_eq!(e.vfx.kindled[0].cell, here, "and it stood where we are");

	e.start_cast(CastTarget::Sphere);
	let climb = e.own_sphere().expect("the climb began");
	assert_eq!(climb.head, here, "the sphere closes about where we are");
}

/// Every standing kindled light keys its own carrier. Two on one key share a `fired` slot and
/// re-place each other every frame from two different cells (the `CARRIER_DEBUG` lesson) — and the
/// counter behind the key is MASKED, so this is the minter's promise rather than the type's. Swept
/// over as many lights as a full server can have burning at once, the bound
/// `the_bank_outlasts_every_light_that_can_burn_at_once` holds the bank against.
#[test]
fn every_standing_kindled_light_keys_its_own_carrier() {
	let mut e = fresh();
	let quickest =
		digblock_core::unlock::channel_ticks(u32::MAX) as f32 * digblock_core::player::TICK;
	let per_caster = (digblock_core::kindle::SECS as f32 / quickest).ceil() as usize;
	let alight = per_caster * digblock_net::protocol::MAX_SNAPSHOT_PLAYERS;
	for i in 0..alight {
		e.kindle_light((i as i32, 70, 0), digblock_core::kindle::SECS as f32);
	}
	let keys: std::collections::HashSet<u16> = e.vfx.kindled.iter().map(|k| k.key).collect();
	assert_eq!(keys.len(), alight, "two lights are wearing one carrier key");
}

/// A connected engine, past the handshake, on a world of the server's.
fn connected() -> (Engine, Box<dyn digblock_net::transport::NetTransport>) {
	use digblock_net::protocol::{PROTOCOL_VERSION, encode_preamble};
	use digblock_net::transport::NetTransport;
	let mut e = fresh();
	let (client_end, mut server_end) = digblock_net::loopback::loopback_pair();
	e.set_transport_with_token(Box::new(client_end), [0; 16]);
	server_end.send_reliable(&encode_preamble(PROTOCOL_VERSION));
	let mut frame = Vec::new();
	digblock_net::codec::write_frame(
		&digblock_net::messages::ServerMsg::Welcome {
			protocol: PROTOCOL_VERSION,
			seed: 11.0,
			player_id: 1,
			spawn: [0.5, 140.5, 0.5],
			server_tick: 60,
			snapshot_divisor: 3,
			session_token: [0; 16],
			sim_version: digblock_core::SIM_VERSION,
			build: "test".into(),
			tod: 0.0,
			spawn_look: [0.0, 0.0, 0.0],
			terrain: digblock_core::worldgen::TerrainVersion::CURRENT.to_wire(),
		},
		&mut frame,
	)
	.expect("encode");
	server_end.send_reliable(&frame);
	e.update(std::time::Duration::from_millis(16));
	assert!(
		e.session.client.as_ref().is_some_and(|n| n.ready()),
		"the handshake did not finish"
	);
	(e, Box::new(server_end))
}

/// A climb still standing under a finished channel: the span the server
/// runs began when `Cast(Sphere)` ARRIVED, so it lands `lag` ticks late.
fn winding(e: &mut Engine, span: u32) {
	let by = e.session.my_player_id;
	e.sphere_climbs.push(SphereClimb {
		by,
		closing: digblock_core::sphere::Closing::new(Vec::new(), span),
		head: digblock_core::kindle::host_cell(e.body_pos()),
		block: matter(),
		prefix: 0,
		placed: 0,
		bill: None,
	});
}

/// A player id that is not ours, derived from the one Welcome gave us so
/// the two can never collide.
fn stranger(e: &Engine) -> digblock_net::protocol::PlayerId {
	e.session.my_player_id.expect("connected") + 1
}

/// Put one server message on the wire and let the engine read it. Costs
/// exactly the tick [`tick`] costs — the tick the message lands on,
/// since `pump_net` runs ahead of the sim ticks in one step.
fn deliver(
	e: &mut Engine,
	server: &mut dyn digblock_net::transport::NetTransport,
	msg: &digblock_net::messages::ServerMsg,
) {
	let mut frame = Vec::new();
	digblock_net::codec::write_frame(msg, &mut frame).expect("encode");
	server.send_reliable(&frame);
	tick(e);
}

/// A head a whole sphere clear of our own — a caster standing off to
/// `side`, whose cells no other plan in the test claims.
fn clear_head(e: &Engine, r: i32, side: i32) -> (i32, i32, i32) {
	let (x, y, z) = digblock_core::kindle::host_cell(e.body_pos());
	(x + side * (2 * r + 2), y, z)
}

/// The sphere a caster's `SphereWinding` names, as this end draws it —
/// the same call the engine makes, so an expectation is the plan itself
/// rather than a count written down.
fn sphere_plan_at(e: &Engine, head: (i32, i32, i32), r: i32) -> Vec<((i32, i32, i32), u16)> {
	digblock_core::sphere::plan(head, r, matter(), |c| e.pipeline.cell_at(c.0, c.1, c.2))
		.expect("the sphere's cells are resident")
}

/// How many of a plan's cells stand written in the world.
fn standing(e: &Engine, plan: &[((i32, i32, i32), u16)]) -> usize {
	plan.iter()
		.filter(|&&(c, v)| e.pipeline.cell_at(c.0, c.1, c.2) == Some(v))
		.count()
}

/// What the plan's cells hold right now — the world a break has to
/// restore, read through the plan so two runs compare cell for cell.
fn cells_of(e: &Engine, plan: &[((i32, i32, i32), u16)]) -> Vec<Option<u16>> {
	plan.iter()
		.map(|&(c, _)| e.pipeline.cell_at(c.0, c.1, c.2))
		.collect()
}

/// The first castable rank and everything derived from it: the radius
/// its sphere spans and the channel it climbs through.
fn sphere_rank() -> (i32, u8, u32) {
	let level = digblock_core::unlock::SPHERE_AT;
	let r = digblock_core::unlock::sphere_radius(level);
	(
		r,
		u8::try_from(r).expect("a castable radius fits the wire"),
		digblock_core::unlock::channel_ticks(level),
	)
}

fn winding_msg(
	by: digblock_net::protocol::PlayerId,
	head: (i32, i32, i32),
	radius: u8,
	span: u32,
) -> digblock_net::messages::ServerMsg {
	digblock_net::messages::ServerMsg::SphereWinding {
		by,
		head: [head.0, head.1, head.2],
		radius,
		span,
		block: matter() as u16,
	}
}

fn ended_msg(
	by: digblock_net::protocol::PlayerId,
	head: (i32, i32, i32),
	radius: u8,
	landed: u32,
	sealed: bool,
) -> digblock_net::messages::ServerMsg {
	digblock_net::messages::ServerMsg::SphereEnded {
		by,
		head: [head.0, head.1, head.2],
		radius,
		landed,
		sealed,
		block: matter() as u16,
	}
}

/// A remote's sphere CLIMBS rather than appearing whole at the seal.
/// `SphereWinding` carries the head, the radius, the span and the block,
/// which is everything `sphere::plan` needs, so a stranger's shell rises
/// through their channel exactly as ours does through ours.
#[test]
fn a_remote_sphere_climbs_rather_than_standing_up_whole() {
	let (mut e, mut server) = connected();
	settle(&mut e);
	let (r, wire_r, span) = sphere_rank();
	let by = stranger(&e);
	let head = clear_head(&e, r, 1);
	let plan = sphere_plan_at(&e, head, r);
	assert!(plan.len() > 10, "the sphere is worth watching climb");
	assert_eq!(standing(&e, &plan), 0, "the plan is already standing");

	deliver(
		&mut e,
		server.as_mut(),
		&winding_msg(by, head, wire_r, span),
	);
	for _ in 0..span / 2 {
		tick(&mut e);
	}
	let half = standing(&e, &plan);
	assert!(
		half > 0 && half < plan.len(),
		"a remote's sphere stood up whole instead of climbing: {half} of {}",
		plan.len()
	);
	let landed = u32::try_from(plan.len()).expect("a plan fits the wire");
	deliver(
		&mut e,
		server.as_mut(),
		&ended_msg(by, head, wire_r, landed, true),
	);
	assert_eq!(standing(&e, &plan), plan.len(), "the seal left holes");
}

/// Two casters at once: each sphere climbs on its own caster's clock,
/// and one ending — either way it ends — leaves the other standing.
#[test]
fn two_spheres_climb_side_by_side_and_one_ending_leaves_the_other() {
	let (mut e, mut server) = connected();
	settle(&mut e);
	let (r, wire_r, span) = sphere_rank();
	let (a, b) = (stranger(&e), stranger(&e) + 1);
	let (head_a, head_b) = (clear_head(&e, r, 1), clear_head(&e, r, -1));
	let (plan_a, plan_b) = (sphere_plan_at(&e, head_a, r), sphere_plan_at(&e, head_b, r));
	assert!(
		plan_a.iter().all(|x| !plan_b.iter().any(|y| y.0 == x.0)),
		"the two spheres overlap — neither could be told apart"
	);

	deliver(
		&mut e,
		server.as_mut(),
		&winding_msg(a, head_a, wire_r, span),
	);
	deliver(
		&mut e,
		server.as_mut(),
		&winding_msg(b, head_b, wire_r, span),
	);
	for _ in 0..span / 3 {
		tick(&mut e);
	}
	assert!(standing(&e, &plan_a) > 0, "A never climbed");
	let b_half = standing(&e, &plan_b);
	assert!(b_half > 0 && b_half < plan_b.len(), "B never climbed");

	// A breaks: A's glass comes down, B keeps climbing.
	deliver(
		&mut e,
		server.as_mut(),
		&ended_msg(a, head_a, wire_r, 0, false),
	);
	assert_eq!(standing(&e, &plan_a), 0, "A's sphere outlived its break");
	assert!(
		standing(&e, &plan_b) >= b_half,
		"A's break took B's glass with it"
	);
	for _ in 0..span {
		tick(&mut e);
	}
	assert_eq!(standing(&e, &plan_b), plan_b.len(), "B never sealed");
	assert_eq!(standing(&e, &plan_a), 0, "A's sphere came back");
}

/// A break is the CASTER's, not the world's: our own channel breaking
/// takes our own sphere down and leaves a stranger's climbing.
#[test]
fn a_broken_channel_takes_our_own_climb_and_leaves_a_remote_standing() {
	let (mut e, mut server) = connected();
	arm_sphere(&mut e);
	let (r, wire_r, span) = sphere_rank();
	let by = stranger(&e);
	let head = clear_head(&e, r, 1);
	let plan = sphere_plan_at(&e, head, r);
	deliver(
		&mut e,
		server.as_mut(),
		&winding_msg(by, head, wire_r, span),
	);

	// Ours, stood up by the server's word about our own cast.
	let me = e.session.my_player_id.expect("connected");
	let mine = digblock_core::kindle::host_cell(e.body_pos());
	let my_plan = sphere_plan_at(&e, mine, r);
	e.start_cast(CastTarget::Sphere);
	deliver(
		&mut e,
		server.as_mut(),
		&winding_msg(me, mine, wire_r, span),
	);
	for _ in 0..span / 3 {
		tick(&mut e);
	}
	let ours = my_plan
		.iter()
		.find(|&&(c, v)| e.pipeline.cell_at(c.0, c.1, c.2) == Some(v))
		.expect("our own sphere never climbed")
		.0;
	// Online our own climb rides the authoritative path, so its cells
	// reach the overlay that mirrors the journal — written with the
	// local funnel instead they would be gone at the first eviction.
	assert!(
		e.cell_edited(ours),
		"our own climb never reached the overlay mirroring the journal"
	);
	let theirs = standing(&e, &plan);
	assert!(theirs > 0, "the stranger's sphere never climbed");

	// A dig hold breaks OUR channel.
	e.input.dig_held = true;
	tick(&mut e);
	assert!(e.cast.is_none(), "the dig hold broke the channel");
	assert_eq!(standing(&e, &my_plan), 0, "our own shell stayed up");
	assert!(
		standing(&e, &plan) >= theirs,
		"our break took the stranger's shell with it"
	);
	assert!(
		e.sphere_climbs.iter().any(|w| w.by == Some(by)),
		"our break took the stranger's climb with it"
	);
}

/// `landed` is the server's own count of what it handed the world, and
/// this end's clock is a round trip out of step with it in either
/// direction — so a break reconciles to the same world whichever way
/// the two disagree. Swept over both, since one direction alone would
/// pass on a client that simply ignored the count.
#[test]
fn a_break_reconciles_whichever_clock_ran_ahead() {
	let mut worlds = Vec::new();
	for ahead in [true, false] {
		let (mut e, mut server) = connected();
		settle(&mut e);
		let (r, wire_r, span) = sphere_rank();
		let by = stranger(&e);
		let head = clear_head(&e, r, 1);
		let plan = sphere_plan_at(&e, head, r);
		let pristine = cells_of(&e, &plan);
		deliver(
			&mut e,
			server.as_mut(),
			&winding_msg(by, head, wire_r, span),
		);
		for _ in 0..span / 2 {
			tick(&mut e);
		}
		// The prefix this end has reached, read off the climb itself.
		let mid = e
			.sphere_climbs
			.iter()
			.find(|w| w.by == Some(by))
			.expect("the climb stands")
			.prefix;
		assert!(mid > 0 && mid < plan.len(), "the climb is partway through");
		let landed = if ahead {
			mid / 2
		} else {
			(mid + plan.len()) / 2
		};
		assert_ne!(
			landed, mid,
			"the two clocks agree — the sweep proves nothing"
		);
		deliver(
			&mut e,
			server.as_mut(),
			&ended_msg(by, head, wire_r, landed as u32, false),
		);
		let after = cells_of(&e, &plan);
		assert_eq!(
			after, pristine,
			"ahead={ahead}: the break left glass behind"
		);
		worlds.push(after);
	}
	assert_eq!(
		worlds[0], worlds[1],
		"the two clocks left two different worlds"
	);
}

/// A stranger's sphere is standing in our world, and none of it is ours
/// to answer for: it must not hold our own channel open past its length
/// waiting for a seal that belongs to somebody else.
#[test]
fn a_remote_climb_does_not_hold_our_own_channel() {
	let (mut e, mut server) = connected();
	settle(&mut e);
	let (r, wire_r, _) = sphere_rank();
	let by = stranger(&e);
	let head = clear_head(&e, r, 1);
	// A climb that outlasts the window, so it is still standing at the
	// tick our own channel has to let go on.
	deliver(
		&mut e,
		server.as_mut(),
		&winding_msg(by, head, wire_r, u32::MAX),
	);
	assert!(
		e.sphere_climbs.iter().any(|w| w.by == Some(by)),
		"the stranger's climb never stood"
	);
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	e.start_cast(CastTarget::Sphere);
	for _ in 0..need {
		tick(&mut e);
	}
	assert!(
		e.cast.is_none(),
		"a stranger's climb held our own channel open"
	);
}

/// Ours among several: the offline award and the refund key on the
/// CASTER, never on whichever climb is standing first. The stranger's
/// climb is planted, because the discriminator is a player id and
/// offline the wire cannot deliver one — which is the whole reason the
/// guard names a caster instead of asking whether anything is climbing.
#[test]
fn the_offline_award_and_refund_find_our_own_climb() {
	use digblock_core::skills::total_xp_for;
	let level = digblock_core::unlock::SPHERE_AT;
	let need = digblock_core::unlock::channel_ticks(level);
	// Offline OUR climb is the one with no caster on it, so any id at
	// all is somebody else's.
	let plant = |e: &mut Engine| {
		let head = digblock_core::kindle::host_cell(e.body_pos());
		e.sphere_climbs.push(SphereClimb {
			by: Some(1),
			closing: digblock_core::sphere::Closing::new(Vec::new(), u32::MAX),
			head,
			block: matter(),
			prefix: 0,
			placed: 0,
			bill: None,
		});
	};
	let armed = || {
		let mut e = fresh();
		let (armed_level, billed) = arm_sphere(&mut e);
		assert_eq!(armed_level, level);
		(e, billed)
	};

	// A seal pays US, with a stranger's climb standing ahead of ours.
	let (mut e, _) = armed();
	plant(&mut e);
	e.start_cast(CastTarget::Sphere);
	assert_eq!(balances(&e, level), vec![0, 0], "our cast paid its fares");
	assert_eq!(e.sphere_climbs.len(), 2, "our own climb never began");
	for _ in 0..=need {
		tick(&mut e);
	}
	assert!(
		e.skills.xp(Skill::Arcana) > total_xp_for(level),
		"the seal went unpaid — the award read a stranger's climb"
	);

	// And a break refunds OUR bill, once, leaving the stranger alone.
	let (mut e, billed) = armed();
	plant(&mut e);
	let (r, ..) = sphere_rank();
	let mine = digblock_core::kindle::host_cell(e.body_pos());
	let my_plan = sphere_plan_at(&e, mine, r);
	e.start_cast(CastTarget::Sphere);
	for _ in 0..need / 2 {
		tick(&mut e);
	}
	assert!(standing(&e, &my_plan) > 0, "our own sphere never climbed");
	e.input.dig_held = true;
	tick(&mut e);
	assert!(e.cast.is_none(), "the dig hold broke the channel");
	assert_eq!(
		balances(&e, level),
		sphere_fares(level).iter().map(|f| f.1).collect::<Vec<_>>(),
		"the break refunded something other than our own fares"
	);
	assert_eq!(
		e.inventory.count(matter()),
		billed,
		"and our own blocks, all of them"
	);
	assert_eq!(standing(&e, &my_plan), 0, "our own shell stayed up");
	assert_eq!(
		e.sphere_climbs.len(),
		1,
		"our own break took the stranger's climb with it"
	);
}

/// The Sphere's channel outlives the caster's own bar. `Cast(Sphere)` goes out as the channel
/// BEGINS, so the server's span starts one upstream latency later and seals that much after the
/// caster's own bar fills — so the channel keeps holding while a climb stands under it, and the
/// seal releases it.
#[test]
fn a_sphere_channel_holds_until_the_seal() {
	let (mut e, _server) = connected();
	arm_sphere(&mut e);
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	let lag = 12;
	e.start_cast(CastTarget::Sphere);
	assert!(e.cast.is_some(), "the channel opened");
	for _ in 0..lag {
		tick(&mut e);
	}
	// The server's word that the climb began, a round trip behind us.
	winding(&mut e, need);
	for _ in 0..need - lag {
		tick(&mut e);
	}
	assert!(
		e.cast.is_some(),
		"the channel let go while the sphere was still climbing"
	);
	// The climb reaches its own end and the seal releases the hold.
	for _ in 0..=lag {
		tick(&mut e);
	}
	assert!(e.sphere_climbs.is_empty(), "the climb sealed");
	assert!(e.cast.is_none(), "and the hold outlived the seal");
}

/// And the hold is BOUNDED. A server-side unwind is silent — nothing
/// comes back to say the sphere is gone — so a caster held on a climb
/// that never ends would be frozen for good.
#[test]
fn a_sphere_channel_never_holds_longer_than_its_grace() {
	let (mut e, _server) = connected();
	arm_sphere(&mut e);
	let need = digblock_core::unlock::channel_ticks(e.skills.level(Skill::Arcana));
	e.start_cast(CastTarget::Sphere);
	// A climb that never completes: the seal will never release this.
	winding(&mut e, u32::MAX);
	let grace = (Engine::SPHERE_SEAL_GRACE_SECS / digblock_core::player::TICK) as u32;
	for _ in 0..need + grace {
		tick(&mut e);
	}
	assert!(e.cast.is_none(), "the hold never ran out");
	// Nothing else about the channel changed: it took its own length
	// plus the bound, and no more.
	let (mut e, _server) = connected();
	arm_sphere(&mut e);
	e.start_cast(CastTarget::Sphere);
	winding(&mut e, u32::MAX);
	for _ in 0..need {
		tick(&mut e);
	}
	assert!(e.cast.is_some(), "the bound cut the hold short");
}

/// Leaving a world takes a climbing sphere down with it. Half a shell
/// left standing is matter the SAVE keeps and a bill the caster paid for
/// nothing — so the unwind runs while this world is still the one under
/// the cells, which is why it hangs off the save's own close rather than
/// the transient reset after the pipeline swap.
#[test]
fn leaving_a_world_takes_a_climbing_sphere_with_it() {
	use digblock_core::block::BlockType;
	use digblock_core::chunk::{cell_id, make_cell};
	let mut e = fresh();
	let (level, billed) = arm_sphere(&mut e);
	let r = digblock_core::unlock::sphere_radius(level);
	let head = digblock_core::kindle::host_cell(e.body_pos());
	let (shell, _) = digblock_core::sphere::sets(head, r);
	let air_cells: Vec<_> = shell
		.iter()
		.copied()
		.filter(|&(x, y, z)| {
			e.pipeline.cell_at(x, y, z).map(cell_id) == Some(BlockType::Air as u16)
		})
		.collect();
	assert!(air_cells.len() > 10, "the shell about spawn is mostly air");
	let wall = make_cell(matter(), 0);
	let built = |e: &Engine| {
		air_cells
			.iter()
			.filter(|&&(x, y, z)| e.pipeline.cell_at(x, y, z) == Some(wall))
			.count()
	};

	e.start_cast(CastTarget::Sphere);
	for _ in 0..digblock_core::unlock::channel_ticks(level) / 2 {
		tick(&mut e);
	}
	assert!(built(&e) > 0, "the shell is climbing");
	assert_eq!(balances(&e, level), vec![0, 0], "the fares are spent");
	assert_eq!(e.inventory.count(matter()), 0, "and the blocks");

	e.close_local_save();
	assert!(e.sphere_climbs.is_empty(), "the climb was abandoned");
	assert_eq!(built(&e), 0, "every block came back down");
	assert_eq!(
		balances(&e, level),
		sphere_fares(level).iter().map(|f| f.1).collect::<Vec<_>>(),
		"and the fares came back"
	);
	assert_eq!(e.inventory.count(matter()), billed, "and the blocks");
}
