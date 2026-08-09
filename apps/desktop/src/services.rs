//! Master-server client services for the menu: server-list fetch, per-server
//! ping probes, and the join flow (mint a one-time token, connect with the
//! listed cert pin). One background thread owns all HTTP; the engine sees
//! only the `MenuServices` channels — it stays wasm-clean and never blocks.
//!
//! HTTP contract (mirrors apps/master/Contracts.cs):
//!   GET  /api/servers → { "servers": [{id, name, url, certHash, players, maxPlayers}] }
//!   POST /api/join { serverId, displayName } → { token, expiresIn, url, certHash }

use digblock_render::menu::{MenuCommand, MenuResponse, MenuServices, ServerEntry};
use serde::Deserialize as _;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
	servers: Vec<ListServer>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListServer {
	id: String,
	name: String,
	url: String,
	cert_hash: String,
	#[serde(deserialize_with = "lenient_count")]
	players: u32,
	#[serde(deserialize_with = "lenient_count")]
	max_players: u32,
}

/// A player count, clamped rather than rejected.
///
/// The master types these as `int` and (until recently) range-checked
/// neither, so a server could register `players: -1`. Decoding straight
/// into `u32` fails the WHOLE `ListResponse`, which means one bad row —
/// hostile or merely buggy — hides every server on the master from every
/// client. A count is display text next to a name; it is not worth that.
fn lenient_count<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
	let n = i64::deserialize(d)?;
	Ok(n.clamp(0, u32::MAX as i64) as u32)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct JoinResponse {
	token: String,
	url: String,
	cert_hash: String,
}

pub fn start(master_url: String) -> MenuServices {
	let (cmd_tx, cmd_rx) = mpsc::channel::<MenuCommand>();
	let (resp_tx, resp_rx) = mpsc::channel::<MenuResponse>();
	std::thread::Builder::new()
		.name("menu-services".into())
		.spawn(move || run(master_url, cmd_rx, resp_tx))
		.expect("spawn menu-services thread");
	MenuServices { cmd_tx, resp_rx }
}

fn run(
	master_url: String,
	cmd_rx: mpsc::Receiver<MenuCommand>,
	resp_tx: mpsc::Sender<MenuResponse>,
) {
	while let Ok(cmd) = cmd_rx.recv() {
		match cmd {
			MenuCommand::RefreshServers => {
				let result = fetch_servers(&master_url);
				let probes: Vec<(String, String, String)> = result
					.as_ref()
					.map(|servers| {
						servers
							.iter()
							.map(|s| (s.id.clone(), s.url.clone(), s.cert_hash.clone()))
							.collect()
					})
					.unwrap_or_default();
				// List first (the menu creates its rows from it), then the
				// pings stream in per server as their probes finish.
				let _ = resp_tx.send(MenuResponse::ServerList(result));
				for (id, url, cert_hash) in probes {
					let resp_tx = resp_tx.clone();
					// No pin, no probe: a ping is not worth an unvalidated
					// connection, and the join would refuse this row anyway.
					let policy = match pin_policy(&cert_hash) {
						Ok(p) => p,
						Err(e) => {
							log::warn!("server {id}: {e}; skipping ping");
							continue;
						}
					};
					digblock_net_native::probe(&url, policy, Duration::from_secs(3), move |ms| {
						let _ = resp_tx.send(MenuResponse::Ping { server_id: id, ms });
					});
				}
			}
			MenuCommand::Join { server_id, name } => {
				let resp = match join(&master_url, &server_id, &name) {
					Ok((transport, token)) => MenuResponse::Connect {
						transport: Box::new(transport),
						token,
					},
					Err(reason) => MenuResponse::JoinFailed { reason },
				};
				let _ = resp_tx.send(resp);
			}
		}
	}
}

/// The pin the master published for a server. A malformed hash is a hard
/// error, never a downgrade.
///
/// This used to fall back to [`CertPolicy::Insecure`] with a `log::warn!`,
/// which turned one bad string — from a hostile master, or from anyone
/// MITM-ing a plain-HTTP `--master-url` — into a session with certificate
/// validation switched off, carrying `Hello.identity` with it. The web
/// filler has always failed closed here (`apps/web/src/services.rs`); the
/// two now agree.
///
/// [`CertPolicy::Insecure`]: digblock_net_native::CertPolicy::Insecure
fn pin_policy(cert_hash: &str) -> Result<digblock_net_native::CertPolicy, String> {
	digblock_net_native::CertPolicy::pinned_from_hex(cert_hash)
		.map_err(|e| format!("bad certHash from master: {e}"))
}

fn fetch_servers(master_url: &str) -> Result<Vec<ServerEntry>, String> {
	let mut res = ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.timeout_global(Some(Duration::from_secs(5)))
			.build(),
	)
	.get(format!("{master_url}/api/servers"))
	.call()
	.map_err(|e| e.to_string())?;
	let list: ListResponse = res.body_mut().read_json().map_err(|e| e.to_string())?;
	Ok(list
		.servers
		.into_iter()
		.map(|s| ServerEntry {
			id: s.id,
			name: s.name,
			url: s.url,
			cert_hash: s.cert_hash,
			players: s.players,
			max_players: s.max_players,
		})
		.collect())
}

fn join(
	master_url: &str,
	server_id: &str,
	name: &str,
) -> Result<(digblock_net_native::ChannelTransport, [u8; 16]), String> {
	let mut res = ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.timeout_global(Some(Duration::from_secs(5)))
			.build(),
	)
	.post(format!("{master_url}/api/join"))
	.send_json(serde_json::json!({ "serverId": server_id, "displayName": name }))
	.map_err(|e| format!("join request failed: {e}"))?;
	let join: JoinResponse = res.body_mut().read_json().map_err(|e| e.to_string())?;
	let token = parse_token(&join.token)?;
	let policy = pin_policy(&join.cert_hash)?;
	let transport = digblock_net_native::connect_with(&join.url, policy);
	Ok((transport, token))
}

/// Decode the master's 32-char hex join token.
///
/// The ASCII-hex check is load-bearing, not belt-and-braces: `hex.len()` is
/// BYTES, so a 32-byte string carrying a multi-byte character (`"€" +
/// 29×"a"`) passes the length gate and then panics on
/// `&hex[i * 2..i * 2 + 2]`, which splits it mid-codepoint. On web that is
/// reachable with no compromise at all — `?master=` rides in a shared link
/// — and the panic aborts the wasm module. `parse_sha256_hex` next door has
/// always filtered first; this now matches it.
fn parse_token(hex: &str) -> Result<[u8; 16], String> {
	if hex.len() != 32 {
		return Err(format!("bad token length {}", hex.len()));
	}
	if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
		return Err("join token is not hex".into());
	}
	let mut token = [0u8; 16];
	for (i, byte) in token.iter_mut().enumerate() {
		*byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
	}
	Ok(token)
}

/// Non-interactive `--join`: list (retrying briefly — scripted flows start
/// the server moments earlier), pick first or by name filter, mint, connect.
pub fn auto_join(
	master_url: &str,
	filter: Option<&str>,
	name: &str,
) -> Result<(digblock_net_native::ChannelTransport, [u8; 16]), String> {
	let deadline = Instant::now() + Duration::from_secs(30);
	let picked = loop {
		match fetch_servers(master_url) {
			Ok(servers) => {
				let found = servers.into_iter().find(|s| match filter {
					Some(f) => s.name.contains(f),
					None => true,
				});
				if let Some(s) = found {
					break s;
				}
			}
			Err(e) => log::debug!("server list not ready: {e}"),
		}
		if Instant::now() >= deadline {
			return Err("no matching server in the master's list".into());
		}
		std::thread::sleep(Duration::from_millis(500));
	};
	log::info!("auto-join: \"{}\" at {}", picked.name, picked.url);
	join(master_url, &picked.id, name)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn token_hex_round_trips() {
		let token = parse_token("000102030405060708090a0b0c0d0e0f").unwrap();
		assert_eq!(token[0], 0);
		assert_eq!(token[15], 0x0f);
		assert!(parse_token("zz0102030405060708090a0b0c0d0e0f").is_err());
		assert!(parse_token("deadbeef").is_err());
	}

	/// A 32-BYTE token containing a multi-byte character used to panic the
	/// slice below the length check. Reproduced before it was fixed.
	#[test]
	fn a_non_ascii_token_is_rejected_not_a_panic() {
		let hex = format!("{}{}", "€", "a".repeat(29));
		assert_eq!(hex.len(), 32, "32 bytes, 30 chars — passes a byte check");
		assert!(parse_token(&hex).is_err());
	}

	/// One bad row must not hide every server on the master. The master
	/// clamps these too; this is the half that protects against a master
	/// that is older, or simply wrong.
	#[test]
	fn a_hostile_player_count_does_not_fail_the_whole_list() {
		let json = r#"{"servers":[
			{"id":"a1","name":"Bad","url":"https://h:4433","certHash":"ab",
			 "players":-1,"maxPlayers":-5},
			{"id":"a2","name":"Good","url":"https://h:4434","certHash":"cd",
			 "players":3,"maxPlayers":8}]}"#;
		let list: ListResponse = serde_json::from_str(json).expect("the list still decodes");
		assert_eq!(list.servers.len(), 2, "and keeps BOTH rows");
		assert_eq!(list.servers[0].players, 0, "the bad count clamps");
		assert_eq!(list.servers[0].max_players, 0);
		assert_eq!(list.servers[1].players, 3, "the good row is untouched");
		// Absurdly large values clamp rather than wrap.
		let big = r#"{"servers":[{"id":"a","name":"B","url":"u","certHash":"c",
			"players":99999999999,"maxPlayers":8}]}"#;
		let list: ListResponse = serde_json::from_str(big).unwrap();
		assert_eq!(list.servers[0].players, u32::MAX);
	}

	#[test]
	fn list_json_parses() {
		let json = r#"{"servers":[{"id":"a1","name":"World","url":"https://h:4433",
			"certHash":"ab","players":3,"maxPlayers":255}]}"#;
		let list: ListResponse = serde_json::from_str(json).unwrap();
		assert_eq!(list.servers[0].name, "World");
		assert_eq!(list.servers[0].max_players, 255);
	}
}
