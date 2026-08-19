use super::*;
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
	}
}

/// The join's loading cover is the hold: `Welcome` no longer closes the
/// Connecting screen itself, so a `Welcome` that arms nothing strands
/// the player on "Joining world..." forever — the timeout escape hatch
/// cannot fire on a hold that was never set.
///
/// This shipped broken because `begin_arrival_hold` asked
/// `self.net.is_some()`, and its only two call sites are `pump_net`
/// arms — where the connection is owned by value (`self.net.take()`)
/// and the field always reads `None`. So drive the real event path
/// rather than calling the helper: a unit test of the helper alone
/// passes against exactly the bug it is meant to catch.
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
		engine.seed, 11.0,
		"the Welcome was actually processed, not swallowed"
	);
	assert!(
		engine.arrival_hold.is_some(),
		"a join must hold the world until the server says the area is real"
	);

	server_end.send_reliable(&frame(&ServerMsg::AreaReady));
	engine.update(Duration::from_millis(16));
	assert!(
		engine.arrival_hold.is_none(),
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
	assert!(engine.arrival_hold.is_some(), "held on arrival");

	// Just past `AREA_SYNC_TIMEOUT_SECS` worth of the frames above.
	let frames = (digblock_net::protocol::AREA_SYNC_TIMEOUT_SECS / 0.016).ceil() as u32 + 1;
	for _ in 0..frames {
		engine.update(Duration::from_millis(16));
	}
	assert!(
		engine.arrival_hold.is_none(),
		"the wait ran out and the world was entered anyway"
	);
}
