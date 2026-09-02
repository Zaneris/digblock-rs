use super::*;
use digblock_core::worldgen::Seed;
use digblock_net::messages::ServerMsg;
use digblock_net::protocol::{PROTOCOL_VERSION, encode_preamble};
use digblock_net::transport::NetTransport;

fn frame(msg: &ServerMsg) -> Vec<u8> {
	let mut out = Vec::new();
	digblock_net::codec::write_frame(msg, &mut out).expect("encode");
	out
}

fn welcome() -> ServerMsg {
	ServerMsg::Welcome {
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
	}
}

/// The join's loading cover is the hold: `Welcome` no longer closes the Connecting screen itself,
/// so a `Welcome` that arms nothing strands the player there forever — the timeout escape hatch
/// cannot fire on a hold that was never set. Drive the real event path rather than calling
/// `begin_arrival_hold`: its `self.session.client.is_some()` guard always reads `None` from
/// `pump_net`'s arms, so a unit test of the helper alone passes against the bug it must catch.
#[test]
fn welcome_arms_the_arrival_hold_and_area_ready_releases_it() {
	let mut config = EngineConfig::desktop();
	config.menu = true;
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);

	let (client_end, mut server_end) = digblock_net::loopback::loopback_pair();
	engine.set_transport_with_token(Box::new(client_end), [0; 16]);
	server_end.send_reliable(&encode_preamble(PROTOCOL_VERSION));
	server_end.send_reliable(&frame(&welcome()));
	engine.update(Duration::from_millis(16));
	assert_eq!(
		engine.seed,
		Seed::current(11.0),
		"the Welcome was actually processed, not swallowed"
	);
	assert!(
		engine.save.arrival_hold.running(),
		"a join must hold the world until the server says the area is real"
	);

	server_end.send_reliable(&frame(&ServerMsg::AreaReady));
	engine.update(Duration::from_millis(16));
	assert!(
		!engine.save.arrival_hold.running(),
		"the marker releases the hold"
	);
}

/// The escape hatch. A server that never sends the marker must not be
/// able to strand a player on the loading screen.
#[test]
fn the_arrival_hold_times_out_without_the_marker() {
	let mut config = EngineConfig::desktop();
	config.menu = true;
	config.workers = 0;
	config.seed = Some(3.0);
	let mut engine = Engine::new(config);

	let (client_end, mut server_end) = digblock_net::loopback::loopback_pair();
	engine.set_transport_with_token(Box::new(client_end), [0; 16]);
	server_end.send_reliable(&encode_preamble(PROTOCOL_VERSION));
	server_end.send_reliable(&frame(&welcome()));
	engine.update(Duration::from_millis(16));
	assert!(engine.save.arrival_hold.running(), "held on arrival");

	// Just past `AREA_SYNC_TIMEOUT_SECS` worth of the frames above.
	let frames = (digblock_net::protocol::AREA_SYNC_TIMEOUT_SECS / 0.016).ceil() as u32 + 1;
	for _ in 0..frames {
		engine.update(Duration::from_millis(16));
	}
	assert!(
		!engine.save.arrival_hold.running(),
		"the wait ran out and the world was entered anyway"
	);
}

/// A server join arms the walkthrough exactly as a save entry does: the join's `Skills` is
/// the moment the zero-XP question has the server's answer. Fresh lights it; any XP leaves it
/// dark. Driven through the real event path, because `swap_world` resets the tutorial and the
/// arm has to happen AFTER that, once the skills land.
#[test]
fn a_server_join_arms_the_walkthrough_for_a_fresh_character() {
	for (xp, expect) in [
		(Vec::new(), true),
		(
			vec![(digblock_core::skills::Skill::Mining as u8, 5u64)],
			false,
		),
	] {
		let mut config = EngineConfig::desktop();
		config.menu = true;
		config.workers = 0;
		config.seed = Some(3.0);
		let mut engine = Engine::new(config);

		let (client_end, mut server_end) = digblock_net::loopback::loopback_pair();
		engine.set_transport_with_token(Box::new(client_end), [0; 16]);
		server_end.send_reliable(&encode_preamble(PROTOCOL_VERSION));
		server_end.send_reliable(&frame(&welcome()));
		engine.update(Duration::from_millis(16));
		assert_eq!(engine.seed, Seed::current(11.0), "joined");
		assert!(
			!engine.tutorial.is_running(),
			"nothing to arm on before the skills land"
		);

		server_end.send_reliable(&frame(&ServerMsg::Skills { xp }));
		engine.update(Duration::from_millis(16));
		assert_eq!(
			engine.tutorial.is_running(),
			expect,
			"the join's Skills decides the walkthrough (fresh = {expect})"
		);
	}
}
