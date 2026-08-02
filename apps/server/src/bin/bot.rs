//! Headless bot client: drives the real digblock-net NetClient over the
//! real wtransport transport — no GPU. Walks a seeded wandering heading,
//! jumps occasionally, digs straight down at a configurable rate. Exits
//! non-zero if the connection drops early or snapshots go missing, so
//! compose soaks can assert on it.

use digblock_core::block::BlockType;
use digblock_core::player::{Player, PlayerIntent};
use digblock_core::raycast::{raycast, REACH};
use digblock_core::worldgen::terrain_height;
use digblock_net::client::NetClientConfig;
use digblock_net::messages::EditKind;
use digblock_net::protocol::TICK_RATE;
use digblock_net::{NetClient, NetEvent};
use glam::{Vec2, Vec3};
use std::time::{Duration, Instant};

struct BotResult {
	snapshots: u32,
	disconnected: Option<String>,
}

fn main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

	let mut url = "https://127.0.0.1:4433".to_string();
	let mut bots: u32 = 1;
	let mut duration_secs: f32 = 30.0;
	let mut edit_rate: f32 = 0.5;

	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		let mut val = || args.next().unwrap_or_default();
		match arg.as_str() {
			"--url" => url = val(),
			"--bots" => bots = val().parse().unwrap_or(1),
			"--duration" => duration_secs = val().parse().unwrap_or(30.0),
			"--edit-rate" => edit_rate = val().parse().unwrap_or(0.5),
			other => {
				eprintln!("unknown arg: {other}");
				eprintln!(
					"usage: bot [--url U] [--bots N] [--duration SECS] [--edit-rate PER_SEC]"
				);
				std::process::exit(2);
			}
		}
	}

	let handles: Vec<_> = (0..bots)
		.map(|i| {
			let url = url.clone();
			std::thread::spawn(move || run_bot(i, &url, duration_secs, edit_rate))
		})
		.collect();

	let mut failed = false;
	// Expect at least half the nominal 20 Hz snapshot stream.
	let min_snapshots = (duration_secs * TICK_RATE as f32 / 3.0 * 0.5) as u32;
	for (i, h) in handles.into_iter().enumerate() {
		match h.join() {
			Ok(r) => {
				if let Some(reason) = r.disconnected {
					log::error!("bot {i}: disconnected early: {reason}");
					failed = true;
				} else if r.snapshots < min_snapshots {
					log::error!("bot {i}: {} snapshots < {min_snapshots}", r.snapshots);
					failed = true;
				} else {
					log::info!("bot {i}: ok, {} snapshots", r.snapshots);
				}
			}
			Err(_) => failed = true,
		}
	}
	std::process::exit(if failed { 1 } else { 0 });
}

fn run_bot(index: u32, url: &str, duration_secs: f32, edit_rate: f32) -> BotResult {
	let transport = digblock_net_native::connect(url);
	let mut client = NetClient::new(
		Box::new(transport),
		NetClientConfig {
			name: format!("bot-{index}"),
			draw_dist: 4,
		},
	);

	let mut player: Option<Player> = None;
	let mut seed = 0.0f64;
	let mut result = BotResult {
		snapshots: 0,
		disconnected: None,
	};
	let mut events = Vec::new();
	let mut next_edit_in = if edit_rate > 0.0 { 2.0 } else { f32::INFINITY };

	// Terrain-only world model: enough for plausible claims and digs.
	// (No caves/trees/edits — off claims are simply not adopted.)
	let block_at = move |seed: f64| {
		move |x: i32, y: i32, z: i32| -> Option<BlockType> {
			Some(if y <= terrain_height(x, z, seed) {
				BlockType::Stone
			} else {
				BlockType::Air
			})
		}
	};

	let tick = Duration::from_secs_f64(1.0 / TICK_RATE as f64);
	let deadline = Instant::now() + Duration::from_secs_f32(duration_secs);
	let mut next_tick = Instant::now();
	while Instant::now() < deadline {
		client.remotes.advance(tick.as_secs_f32());
		events.clear();
		client.poll(&mut events, tick.as_secs_f32());
		for ev in &events {
			match ev {
				NetEvent::Welcome { seed: s, spawn, .. } => {
					seed = *s;
					player = Some(Player::new(*spawn));
					log::info!("bot {index}: welcomed, spawn {spawn}");
				}
				NetEvent::Refused { reason } => {
					result.disconnected = Some(format!("refused: {reason}"));
					return result;
				}
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => {
					if let Some(p) = player.as_mut() {
						result.snapshots += 1;
						let world = block_at(seed);
						client
							.predictor
							.reconcile(p, *acked_input_tick, *me, &world);
					}
				}
				NetEvent::Disconnected { reason } => {
					result.disconnected = Some(reason.clone());
					return result;
				}
				NetEvent::Edit { .. }
				| NetEvent::EditRejected(_)
				| NetEvent::PlayerJoined { .. }
				| NetEvent::PlayerLeft { .. } => {}
			}
		}

		if let Some(p) = player.as_mut() {
			// Wander: slow seeded heading drift, hop every ~4 s.
			let t = client.predictor.current_tick();
			let yaw = index as f32 * 1.7 + t as f32 * 0.004;
			let dir = Vec3::new(yaw.sin(), -0.1, yaw.cos()).normalize();
			let intent = PlayerIntent {
				move_axis: Vec2::new(0.0, 1.0),
				jump: t % (TICK_RATE * 4) == index % 60,
			};
			let world = block_at(seed);
			client.predictor.predict(p, intent, dir, &world);
			client.send_inputs();

			next_edit_in -= tick.as_secs_f32();
			if next_edit_in <= 0.0 {
				next_edit_in = 1.0 / edit_rate;
				let down = Vec3::NEG_Y;
				if let Some(hit) = raycast(p.pos, down, REACH, &world) {
					let seq = client.edits.begin(hit.cell, hit.block, BlockType::Air, t);
					client.send_edit(seq, t, p.pos, down, EditKind::Dig, hit.cell, (0, 0, 0));
				}
			}
		}

		next_tick += tick;
		let now = Instant::now();
		if now < next_tick {
			std::thread::sleep(next_tick - now);
		} else {
			next_tick = now;
		}
	}
	result
}
