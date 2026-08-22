//! Headless bot client: drives the real digblock-net NetClient over the
//! real wtransport transport — no GPU. Walks a seeded wandering heading,
//! jumps occasionally, and runs the real timed-dig flow (DigStart, hold
//! aim down for the block's hardness, then the completing Dig) at a
//! configurable rate. Exits non-zero if the connection drops early or
//! snapshots go missing, so compose soaks can assert on it.

use digblock_core::block::BlockType;
use digblock_core::player::{Player, PlayerIntent};
use digblock_core::raycast::{REACH, raycast};
use digblock_core::worldgen::terrain_height;
use digblock_net::client::NetClientConfig;
use digblock_net::messages::EditKind;
use digblock_net::protocol::TICK_RATE;
use digblock_net::{NetClient, NetEvent};
use glam::{Vec2, Vec3};
use std::time::{Duration, Instant};

#[derive(Default)]
struct BotResult {
	snapshots: u32,
	disconnected: Option<String>,
	edits_rejected: u32,
	/// Connection health at the end of the run — the same numbers the
	/// desktop overlay's `net` line shows, so a soak reports something you
	/// can act on rather than only pass/fail.
	net: digblock_core::metrics::NetStats,
}

fn main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

	let mut master_url = std::env::var("DIGBLOCK_MASTER_URL")
		.unwrap_or_else(|_| "http://127.0.0.1:5200".to_string());
	let mut server_filter: Option<String> = None;
	let mut bots: u32 = 1;
	let mut duration_secs: f32 = 30.0;
	let mut edit_rate: f32 = 0.5;

	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		let mut val = || args.next().unwrap_or_default();
		match arg.as_str() {
			"--master-url" => master_url = val(),
			"--server" => server_filter = Some(val()),
			"--bots" => bots = val().parse().unwrap_or(1),
			"--duration" => duration_secs = val().parse().unwrap_or(30.0),
			"--edit-rate" => edit_rate = val().parse().unwrap_or(0.5),
			other => {
				eprintln!("unknown arg: {other}");
				eprintln!(
					"usage: bot [--master-url U] [--server NAME_FILTER] [--bots N] \
					 [--duration SECS] [--edit-rate PER_SEC]"
				);
				std::process::exit(2);
			}
		}
	}
	let master_url = master_url.trim_end_matches('/').to_string();

	// Pick the target server from the master's list, like a real client
	// would. Retry briefly: in compose soaks the game server may still be
	// registering when the bots come up.
	let server = pick_server(&master_url, server_filter.as_deref()).unwrap_or_else(|e| {
		log::error!("no joinable server via {master_url}: {e}");
		std::process::exit(1);
	});
	log::info!(
		"joining \"{}\" at {} via {master_url}",
		server.name,
		server.url
	);

	let handles: Vec<_> = (0..bots)
		.map(|i| {
			let master_url = master_url.clone();
			let server = server.clone();
			std::thread::spawn(move || run_bot(i, &master_url, &server, duration_secs, edit_rate))
		})
		.collect();

	let mut failed = false;
	// Expect at least half the nominal snapshot stream.
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
					log::info!(
						"bot {i}: ok, ping {:.1}ms, rtt {:.1}+-{:.1}ms, dn {:.1} up {:.1} kB/s, \
						 {:.1} snap/s, {:.2} replay/s, {} snapshots, {} edit rejections",
						r.net.wire_rtt_ms,
						r.net.rtt_ms,
						r.net.jitter_ms,
						r.net.down_kbps,
						r.net.up_kbps,
						r.net.snapshots_per_sec,
						r.net.replays_per_sec,
						r.snapshots,
						r.edits_rejected
					);
				}
			}
			Err(_) => failed = true,
		}
	}
	std::process::exit(if failed { 1 } else { 0 });
}

#[derive(Clone)]
struct ServerPick {
	id: String,
	name: String,
	url: String,
	cert_hash: String,
}

/// List servers from the master (retrying until a deadline) and pick the
/// first, or the first whose name contains `filter`.
fn pick_server(master_url: &str, filter: Option<&str>) -> Result<ServerPick, String> {
	let deadline = Instant::now() + Duration::from_secs(30);
	let mut last_err = String::from("empty server list");
	loop {
		match ureq::get(format!("{master_url}/api/servers")).call() {
			Ok(mut res) => match res.body_mut().read_json::<serde_json::Value>() {
				Ok(list) => {
					let found = list["servers"].as_array().and_then(|servers| {
						servers.iter().find(|s| match filter {
							Some(f) => s["name"].as_str().is_some_and(|n| n.contains(f)),
							None => true,
						})
					});
					if let Some(s) = found {
						return Ok(ServerPick {
							id: s["id"].as_str().unwrap_or_default().to_string(),
							name: s["name"].as_str().unwrap_or_default().to_string(),
							url: s["url"].as_str().unwrap_or_default().to_string(),
							cert_hash: s["certHash"].as_str().unwrap_or_default().to_string(),
						});
					}
				}
				Err(e) => last_err = format!("bad list response: {e}"),
			},
			Err(e) => last_err = e.to_string(),
		}
		if Instant::now() >= deadline {
			return Err(last_err);
		}
		std::thread::sleep(Duration::from_millis(500));
	}
}

/// Sign a bot in through the master's development-only endpoint.
///
/// Bots cannot open a browser, so they cannot do the real thing. The
/// endpoint is honoured only when the master runs with
/// `ASPNETCORE_ENVIRONMENT=Development` and `Master:Dev:AuthEnabled`, and a
/// master with it set anywhere else refuses to boot — so this cannot reach
/// a production master even by accident.
fn dev_sign_in(master_url: &str, subject: &str) -> Result<String, String> {
	let mut res = ureq::post(format!("{master_url}/api/dev/signin"))
		.send_json(serde_json::json!({ "subject": subject, "platform": "bot" }))
		.map_err(|e| e.to_string())?;
	if res.status().as_u16() != 200 {
		return Err(format!(
			"http {} — is the master running with Master__Dev__AuthEnabled=true?",
			res.status()
		));
	}
	let body: serde_json::Value = res.body_mut().read_json().map_err(|e| e.to_string())?;
	body["sessionToken"]
		.as_str()
		.map(str::to_string)
		.ok_or_else(|| "no sessionToken in dev sign-in response".into())
}

/// Mint this bot's one-time token (tokens are single-use, one per bot).
fn mint_token(master_url: &str, server_id: &str, session: &str) -> Result<[u8; 16], String> {
	// No displayName any more: the master looks up the account's own name.
	let mut res = ureq::post(format!("{master_url}/api/join"))
		.header("authorization", &format!("Bearer {session}"))
		.send_json(serde_json::json!({ "serverId": server_id }))
		.map_err(|e| e.to_string())?;
	let join: serde_json::Value = res.body_mut().read_json().map_err(|e| e.to_string())?;
	let hex = join["token"].as_str().ok_or("no token in join response")?;
	if hex.len() != 32 {
		return Err(format!("bad token length {}", hex.len()));
	}
	// Byte length is not char length: without this the slice below splits a
	// multi-byte character and panics (same fix as the two service fillers).
	if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
		return Err("join token is not hex".into());
	}
	let mut token = [0u8; 16];
	for (i, byte) in token.iter_mut().enumerate() {
		*byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
	}
	Ok(token)
}

fn run_bot(
	index: u32,
	master_url: &str,
	server: &ServerPick,
	duration_secs: f32,
	edit_rate: f32,
) -> BotResult {
	// One dev account per bot index, so a soak exercises the persistence
	// path the way real players do: the same bot rejoining finds its own
	// record, and different bots never share one.
	let name = format!("bot-{index}");
	let session = match dev_sign_in(master_url, &name) {
		Ok(session) => session,
		Err(e) => {
			return BotResult {
				disconnected: Some(format!("dev sign-in failed: {e}")),
				..Default::default()
			};
		}
	};
	let token = match mint_token(master_url, &server.id, &session) {
		Ok(token) => token,
		Err(e) => {
			return BotResult {
				disconnected: Some(format!("token mint failed: {e}")),
				..Default::default()
			};
		}
	};
	let policy = digblock_net_native::CertPolicy::pinned_from_hex(&server.cert_hash)
		.unwrap_or(digblock_net_native::CertPolicy::Insecure);
	let transport = digblock_net_native::connect_with(&server.url, policy);
	let mut client = NetClient::new(
		Box::new(transport),
		NetClientConfig {
			name,
			draw_dist: 4,
			token,
			legacy_identity: None,
		},
	);

	let mut player: Option<Player> = None;
	let mut seed = 0.0f64;
	let mut result = BotResult::default();
	let mut events = Vec::new();
	let mut next_edit_in = if edit_rate > 0.0 { 2.0 } else { f32::INFINITY };
	// In-progress timed dig: target cell, its block, and the predicted
	// tick the hold satisfies the server's hardness gate.
	let mut digging: Option<((i32, i32, i32), BlockType, u32)> = None;
	// A bot never earns XP, so it asks the skill gates as a fresh player.
	let bot_skills = digblock_core::skills::SkillSet::default();

	// Terrain-only world model: enough for plausible claims and digs.
	// (No caves/trees/edits — off claims are simply not adopted.)
	let block_at = move |seed: f64| {
		move |x: i32, y: i32, z: i32| -> Option<u16> {
			Some(if y <= terrain_height(x, z, seed) {
				BlockType::Stone as u16
			} else {
				BlockType::Air as u16
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
						let outcome = client
							.predictor
							.reconcile(p, *acked_input_tick, *me, &world);
						// The ack IS the RTT sample.
						client.note_reconciled(outcome);
					}
				}
				NetEvent::Disconnected { reason } => {
					result.disconnected = Some(reason.clone());
					return result;
				}
				NetEvent::DigRefused { cell, reason } => {
					log::debug!("bot {index}: dig refused at {cell:?} ({reason:?})");
				}
				NetEvent::EditRejected { edit, reason } => {
					result.edits_rejected += 1;
					log::debug!("bot {index}: edit rejected at {:?} ({reason:?})", edit.cell);
				}
				NetEvent::Teleported { pos, look: _ } => {
					// Server-side return to spawn (bots never request one,
					// but the client contract is to hard-adopt it).
					if let Some(p) = player.as_mut() {
						p.set_state(digblock_core::player::PlayerState {
							pos: *pos,
							vel: Vec3::ZERO,
						});
					}
				}
				NetEvent::Edit { .. }
				| NetEvent::Inventory { .. }
				| NetEvent::PlayerJoined { .. }
				| NetEvent::PlayerLeft { .. }
				| NetEvent::Chat { .. }
				| NetEvent::TimeOfDay { .. }
				| NetEvent::AdminCommands { .. }
				| NetEvent::AdminReply(_)
				| NetEvent::Layout { .. }
				| NetEvent::Skills { .. }
				| NetEvent::XpGain { .. }
				| NetEvent::LevelUp { .. }
				// Bots have no camera to hold and no world to render; they
				// simulate against the server's answers either way.
				| NetEvent::Waystones { .. }
				| NetEvent::CastRefused { .. }
				| NetEvent::Kindled { .. }
				| NetEvent::AreaReady => {}
			}
		}

		if let Some(p) = player.as_mut() {
			let t = client.predictor.current_tick();
			let world = block_at(seed);
			let down = Vec3::NEG_Y;

			// Digging holds still and aims straight down; wandering
			// otherwise (slow seeded heading drift, with a periodic hop).
			let (dir, intent) = if digging.is_some() {
				(down, PlayerIntent::default())
			} else {
				let yaw = index as f32 * 1.7 + t as f32 * 0.004;
				(
					Vec3::new(yaw.sin(), -0.1, yaw.cos()).normalize(),
					PlayerIntent {
						move_axis: Vec2::new(0.0, 1.0),
						jump: t % (TICK_RATE * 4) == index % 60,
					},
				)
			};
			client.predictor.predict(p, intent, dir, &world);
			client.send_inputs();

			match digging {
				Some((cell, block, complete_at)) if t >= complete_at => {
					digging = None;
					next_edit_in = 1.0 / edit_rate;
					// Confirm the hold survived (we stood still, but the
					// landing after a hop can shift the hit cell).
					if raycast(p.pos, down, REACH, &world).is_some_and(|h| h.cell == cell) {
						let seq = client.edits.begin(
							cell,
							block as u16,
							BlockType::Air as u16,
							t,
							// `dig_drop` and not `dig_credit`: a bot only ever
							// digs terrain it found, never a cell anybody
							// wrote, so the pristine answer IS its answer and
							// it needs no edit map of its own to know that.
							block.dig_drop().map(|d| (d, 1)),
						);
						client.send_edit(seq, t, p.pos, down, EditKind::Dig, cell, (0, 0, 0), 0);
					} else {
						client.send_dig_cancel();
					}
				}
				Some(_) => {}
				None => {
					next_edit_in -= tick.as_secs_f32();
					if next_edit_in <= 0.0 {
						next_edit_in = f32::INFINITY; // rearmed on completion
						if let Some(hit) = raycast(p.pos, down, REACH, &world) {
							// Same gates as the server, asked the same way — a
							// bot that has dug down to bedrock (or onto
							// level-gated rock, or somebody's craft-only
							// build; bots run unskilled) must skip it,
							// not retry forever.
							if hit.block.dig_hardness(hit.cell.1).is_some()
								&& digblock_core::unlock::dig_blocked(hit.block, &bot_skills)
									.is_none()
							{
								let hardness = hit.block.dig_hardness(hit.cell.1).unwrap();
								client.send_dig_start(hit.cell, 0);
								// Family-scaled hold (bots assume they are
								// unskilled — a leveled-up bot just
								// over-holds), plus a margin over the
								// server's grace.
								let need =
									digblock_core::skills::dig_required(hit.block, hardness, 1);
								digging = Some((hit.cell, hit.block, t + need + 2));
							} else {
								next_edit_in = 1.0 / edit_rate;
							}
						} else {
							next_edit_in = 1.0 / edit_rate;
						}
					}
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
	// Snapshot the connection's health as it stood at the end of the run.
	result.net = client.stats();
	result
}
