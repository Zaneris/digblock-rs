//! Master-server client services for the menu: sign-in, the account's name,
//! server-list fetch, per-server ping probes, and the join flow. One background
//! thread owns all HTTP; the engine sees only the `MenuServices` channels — it
//! stays wasm-clean and never blocks.
//!
//! This thread is also the ONLY place the session token lives. The menu holds a
//! name and an account id, both public; the engine holds neither.
//!
//! HTTP contract (mirrors apps/master/Contracts.cs):
//!   GET  /api/servers → { "servers": [{id, name, url, certHash, …}] }
//!   POST /api/join { serverId } + Bearer session → { token, url, certHash }
//!   GET  /api/hiscores/me + Bearer session → { "ranks": [{skill, rank, level, xp}] }

use crate::auth::{self, Session};
use digblock_client::{
	AccountView, MenuCommand, MenuResponse, MenuServices, RankView, ServerEntry,
};
use digblock_net::protocol::Compat;
use serde::Deserialize as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn view(session: &Session) -> AccountView {
	AccountView {
		id: session.account_id.clone(),
		name: session.display_name.clone(),
	}
}

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
	#[serde(default, deserialize_with = "lenient_version")]
	protocol_version: u16,
	#[serde(default, deserialize_with = "lenient_version")]
	sim_version: u16,
	/// Absent on a master predating the board, which reads as "does not count".
	#[serde(default)]
	reports_skills: bool,
}

/// A player count, clamped rather than rejected.
///
/// The master types these as `int`, so a server can register `players: -1`. Decoding straight into
/// `u32` fails the WHOLE `ListResponse`, so one bad row — hostile or merely buggy — hides every
/// server from every client, and a count is display text next to a name.
fn lenient_count<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
	let n = i64::deserialize(d)?;
	Ok(n.clamp(0, u32::MAX as i64) as u32)
}

/// A version, clamped rather than rejected — `lenient_count`'s argument, plus one of its own: an
/// unreadable version must read as INCOMPATIBLE rather than as ours, and both the `0` a master
/// predating the field would leave and a clamped ceiling do.
fn lenient_version<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
	let n = i64::deserialize(d)?;
	Ok(n.clamp(0, u16::MAX as i64) as u16)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct JoinResponse {
	token: String,
	url: String,
	cert_hash: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RanksResponse {
	ranks: Vec<RankRow>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RankRow {
	skill: String,
	rank: u32,
	level: u32,
	xp: u64,
}

/// A board row as the screen wants it. A slug the client cannot name — a
/// skill from a newer master — is dropped rather than failing the answer.
fn rank_view(row: RankRow) -> Option<RankView> {
	let skill = match row.skill.as_str() {
		"overall" => None,
		slug => Some(
			digblock_core::skills::Skill::ALL
				.into_iter()
				.find(|s| s.name().eq_ignore_ascii_case(slug))?,
		),
	};
	Some(RankView {
		skill,
		rank: row.rank,
		level: row.level,
		xp: row.xp,
	})
}

pub(crate) fn start(master_url: String, session: Option<Session>) -> MenuServices {
	let (cmd_tx, cmd_rx) = mpsc::channel::<MenuCommand>();
	let (resp_tx, resp_rx) = mpsc::channel::<MenuResponse>();
	std::thread::Builder::new()
		.name("menu-services".into())
		.spawn(move || run(master_url, session, cmd_rx, resp_tx))
		.expect("spawn menu-services thread");
	MenuServices { cmd_tx, resp_rx }
}

fn run(
	master_url: String,
	session: Option<Session>,
	cmd_rx: mpsc::Receiver<MenuCommand>,
	resp_tx: mpsc::Sender<MenuResponse>,
) {
	let path = auth::session_path();
	// Rotate the stored session before anything else needs it. Refusal
	// means the token is gone — expired, revoked from another device, or
	// replayed — so drop it locally rather than discovering that at the
	// first join.
	let restored = session.and_then(|s| match auth::refresh(&master_url, &s.token) {
		Ok(fresh) => {
			auth::store(&path, &fresh);
			Some(fresh)
		}
		Err(e) => {
			log::info!("stored session is no longer valid ({e}) — signed out");
			auth::clear(&path);
			None
		}
	});
	let _ = resp_tx.send(MenuResponse::Account(restored.as_ref().map(view)));

	// Shared rather than owned by this loop, because a sign-in runs on its
	// own thread (this one is a blocking recv) and is the one command that
	// produces a session.
	let session = Arc::new(std::sync::Mutex::new(restored));
	// Shared with an in-flight sign-in thread so Cancel can end its wait.
	let cancel = Arc::new(AtomicBool::new(false));

	while let Ok(cmd) = cmd_rx.recv() {
		match cmd {
			MenuCommand::RefreshServers => {
				let result = fetch_servers(&master_url);
				let probes: Vec<(String, String, String)> = result
					.as_ref()
					.map(|servers| {
						servers
							.iter()
							// Same argument as the missing-pin skip below:
							// the join would refuse this row, so its
							// latency is not worth a connection.
							.filter(|s| s.compat() == Compat::Joinable)
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
					digblock_net_native::probe(&url, policy, Duration::from_secs(3), move |ping| {
						let _ = resp_tx.send(MenuResponse::Ping {
							server_id: id,
							ping,
						});
					});
				}
			}
			MenuCommand::Join { server_id } => {
				let token = session
					.lock()
					.expect("session mutex")
					.as_ref()
					.map(|s| s.token.clone());
				let resp = match token {
					Some(token) => match join(&master_url, &server_id, &token) {
						Ok((transport, token)) => MenuResponse::Connect {
							transport: Box::new(transport),
							token,
						},
						Err(reason) => MenuResponse::JoinFailed { reason },
					},
					None => MenuResponse::JoinFailed {
						reason: "not signed in".into(),
					},
				};
				let _ = resp_tx.send(resp);
			}

			// Sign-in gets its own thread. This loop is a blocking
			// `recv()` that serves one command at a time, and a sign-in
			// waits on a human in a browser — running it here would freeze
			// the server list and every other command for up to three
			// minutes.
			MenuCommand::SignIn => {
				cancel.store(false, Ordering::Relaxed);
				let master_url = master_url.clone();
				let resp_tx = resp_tx.clone();
				let cancel = cancel.clone();
				let session = session.clone();
				std::thread::Builder::new()
					.name("menu-signin".into())
					.spawn(move || match auth::sign_in(&master_url, &cancel) {
						Ok(s) => {
							auth::store(&auth::session_path(), &s);
							let account = view(&s);
							*session.lock().expect("session mutex") = Some(s);
							let _ = resp_tx.send(MenuResponse::Account(Some(account)));
						}
						Err(reason) => {
							let _ = resp_tx.send(MenuResponse::SignInFailed { reason });
						}
					})
					.expect("spawn menu-signin thread");
			}
			MenuCommand::CancelSignIn => cancel.store(true, Ordering::Relaxed),

			// Answered inline, which is only safe because the MENU keeps at
			// most one of these outstanding (`check_typed_name`). This loop
			// is a blocking `recv()`, so one queued check per keystroke
			// would put a run of 10 s-timeout GETs ahead of the Join the
			// player presses next.
			MenuCommand::CheckName { name } => {
				let token = session
					.lock()
					.expect("session mutex")
					.as_ref()
					.map(|s| s.token.clone())
					.unwrap_or_default();
				let (available, reason) = auth::check_name(&master_url, &token, &name);
				let _ = resp_tx.send(MenuResponse::NameAvailable {
					name,
					available,
					reason,
				});
			}
			MenuCommand::SetName { name } => {
				let mut guard = session.lock().expect("session mutex");
				let result = match guard.as_ref() {
					Some(s) => auth::set_name(&master_url, &s.token, &name),
					None => Err("not signed in".into()),
				};
				if let (Ok(name), Some(s)) = (&result, guard.as_mut()) {
					s.display_name = Some(name.clone());
					auth::store(&auth::session_path(), s);
				}
				drop(guard);
				let _ = resp_tx.send(MenuResponse::NameSet(result));
			}
			MenuCommand::FetchRanks => {
				let token = session
					.lock()
					.expect("session mutex")
					.as_ref()
					.map(|s| s.token.clone());
				let result = match token {
					Some(token) => fetch_ranks(&master_url, &token),
					None => Err("not signed in".into()),
				};
				let _ = resp_tx.send(MenuResponse::Ranks(result));
			}
			MenuCommand::SignOut => {
				// Cancel any sign-in still waiting on a browser, or its
				// eventual success would sign us straight back in.
				cancel.store(true, Ordering::Relaxed);
				if let Some(s) = session.lock().expect("session mutex").take() {
					auth::sign_out(&master_url, &s.token);
				}
				auth::clear(&auth::session_path());
				let _ = resp_tx.send(MenuResponse::Account(None));
			}
		}
	}
}

/// The pin the master published for a server. A malformed hash is a hard error, never a downgrade:
/// falling back to an insecure policy turns one bad string — from a hostile master, or from anyone
/// MITM-ing a plain-HTTP `--master-url` — into a session with certificate validation switched off,
/// carrying `Hello.identity` with it. `apps/web/src/services.rs` fails closed the same way.
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
			protocol_version: s.protocol_version,
			sim_version: s.sim_version,
			reports_skills: s.reports_skills,
		})
		.collect())
}

/// Where the signed-in account stands on the master's boards.
fn fetch_ranks(master_url: &str, session: &str) -> Result<Vec<RankView>, String> {
	let mut res = ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.timeout_global(Some(Duration::from_secs(5)))
			.build(),
	)
	.get(format!("{master_url}/api/hiscores/me"))
	.header("authorization", &format!("Bearer {session}"))
	.call()
	.map_err(|e| e.to_string())?;
	let body: RanksResponse = res.body_mut().read_json().map_err(|e| e.to_string())?;
	Ok(body.ranks.into_iter().filter_map(rank_view).collect())
}

/// Mint a join token and dial the server.
///
/// No display name goes up: the master reads it off the authenticated
/// account. That is what makes the name on the far side something the
/// server can trust rather than something the joiner asserted.
fn join(
	master_url: &str,
	server_id: &str,
	session: &str,
) -> Result<(digblock_net_native::ChannelTransport, [u8; 16]), String> {
	let mut res = ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.timeout_global(Some(Duration::from_secs(5)))
			.http_status_as_error(false)
			.build(),
	)
	.post(format!("{master_url}/api/join"))
	.header("authorization", &format!("Bearer {session}"))
	.send_json(serde_json::json!({ "serverId": server_id }))
	.map_err(|e| format!("join request failed: {e}"))?;
	if res.status().as_u16() != 200 {
		let status = res.status().as_u16();
		return Err(res
			.body_mut()
			.read_json::<serde_json::Value>()
			.ok()
			.and_then(|v| v["error"].as_str().map(str::to_string))
			.unwrap_or_else(|| match status {
				401 => "your sign-in has expired — sign in again".into(),
				404 => "that server is no longer listed".into(),
				_ => format!("join refused (http {status})"),
			}));
	}
	let join: JoinResponse = res.body_mut().read_json().map_err(|e| e.to_string())?;
	let token = parse_token(&join.token)?;
	let policy = pin_policy(&join.cert_hash)?;
	let transport = digblock_net_native::connect_with(&join.url, policy);
	Ok((transport, token))
}

/// Decode the master's hex join token.
///
/// The ASCII-hex check is load-bearing: `hex.len()` is BYTES, so a string of the right byte length
/// carrying a multi-byte character clears the length gate and then panics splitting a codepoint in
/// the slice below. Reachable with no compromise at all — `?master=` rides in a shared link.
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
/// `session` is a session token — scripted runs get one from
/// `--dev-signin`, since they cannot open a browser.
pub(crate) fn auto_join(
	master_url: &str,
	filter: Option<&str>,
	session: &str,
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
	join(master_url, &picked.id, session)
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

	/// A token of the right BYTE length carrying a multi-byte character
	/// used to panic the slice below the length check. Reproduced before
	/// it was fixed.
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

	/// The version fields, whose failure mode is the opposite of a count's:
	/// a row that cannot be read must land OUTSIDE the window rather than
	/// inside it. Both ways in are covered — a master predating the fields
	/// leaves them absent, and a `u16` cast without the clamp WRAPS, which
	/// is what the third row is: a number chosen to come out as this
	/// build's own protocol version if the clamp is ever dropped.
	#[test]
	fn an_unreadable_version_reads_as_incompatible_rather_than_as_ours() {
		use digblock_net::protocol::{Compat, PROTOCOL_VERSION, compat};
		let wraps_onto_us = u16::MAX as i64 + 1 + PROTOCOL_VERSION as i64;
		let json = format!(
			r#"{{"servers":[
			{{"id":"a1","name":"Old","url":"u","certHash":"c","players":0,"maxPlayers":8}},
			{{"id":"a2","name":"Now","url":"u","certHash":"c","players":0,"maxPlayers":8,
			 "protocolVersion":{PROTOCOL_VERSION},"simVersion":7}},
			{{"id":"a3","name":"Junk","url":"u","certHash":"c","players":0,"maxPlayers":8,
			 "protocolVersion":{wraps_onto_us},"simVersion":-1}}]}}"#
		);
		let list: ListResponse = serde_json::from_str(&json).expect("the list still decodes");
		// Absent: the row parses, and lands outside the window.
		assert_eq!(list.servers[0].protocol_version, 0);
		assert_ne!(compat(0, 0), Compat::Joinable);
		// Present: both keys reach their field, which is the whole of what
		// this parser owes the verdict.
		assert_eq!(list.servers[1].protocol_version, PROTOCOL_VERSION);
		assert_eq!(list.servers[1].sim_version, 7);
		// Wrapped: clamped to the ceiling instead of onto us.
		assert_ne!(list.servers[2].protocol_version, PROTOCOL_VERSION);
		assert_eq!(list.servers[2].protocol_version, u16::MAX);
		assert_eq!(list.servers[2].sim_version, 0);
	}
}
