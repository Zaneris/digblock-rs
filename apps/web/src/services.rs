//! Master-server client services for the menu — the fetch()-based mirror
//! of apps/desktop/src/services.rs (same HTTP contract, same
//! `MenuServices` channel seam; the engine can't tell them apart). One
//! spawn_local task owns all HTTP; std mpsc is single-thread-fine on
//! wasm. Pings time a throwaway WebTransport handshake per server
//! (`digblock_net_web::probe`) — the same measurement as the native
//! QUIC probe.
//!
//! HTTP contract (mirrors apps/master/Contracts.cs):
//!   GET  /api/servers → { "servers": [{id, name, url, certHash, players, maxPlayers}] }
//!   POST /api/join { serverId, displayName } → { token, expiresIn, url, certHash }

use std::sync::mpsc;

use digblock_render::menu::{MenuCommand, MenuResponse, MenuServices, ServerEntry};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

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
	players: u32,
	max_players: u32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct JoinResponse {
	token: String,
	url: String,
	cert_hash: String,
}

/// Start the service task. Scripted joins are the menu's job
/// (`EngineConfig::auto_join`) — the service layer is identical for
/// interactive and scripted flows.
pub fn start(master_url: String) -> MenuServices {
	let (cmd_tx, cmd_rx) = mpsc::channel::<MenuCommand>();
	let (resp_tx, resp_rx) = mpsc::channel::<MenuResponse>();
	wasm_bindgen_futures::spawn_local(run(master_url, cmd_rx, resp_tx));
	MenuServices { cmd_tx, resp_rx }
}

async fn run(
	master_url: String,
	cmd_rx: mpsc::Receiver<MenuCommand>,
	resp_tx: mpsc::Sender<MenuResponse>,
) {
	loop {
		while let Ok(cmd) = cmd_rx.try_recv() {
			match cmd {
				MenuCommand::RefreshServers => {
					let result = fetch_servers(&master_url).await;
					let probes: Vec<(String, String, String)> = result
						.as_ref()
						.map(|servers| {
							servers
								.iter()
								.map(|s| (s.id.clone(), s.url.clone(), s.cert_hash.clone()))
								.collect()
						})
						.unwrap_or_default();
					// List first (the menu creates its rows from it), then
					// the pings stream in per server as handshakes finish.
					let _ = resp_tx.send(MenuResponse::ServerList(result));
					for (id, url, cert_hash) in probes {
						let Ok(hash) = digblock_net_web::parse_sha256_hex(&cert_hash) else {
							let _ = resp_tx.send(MenuResponse::Ping {
								server_id: id,
								ms: None,
							});
							continue;
						};
						let resp_tx = resp_tx.clone();
						digblock_net_web::probe(&url, hash, 3000, move |ms| {
							let _ = resp_tx.send(MenuResponse::Ping { server_id: id, ms });
						});
					}
				}
				MenuCommand::Join { server_id, name } => {
					let resp = join_resp(&master_url, &server_id, &name).await;
					let _ = resp_tx.send(resp);
				}
			}
		}
		sleep_ms(150).await;
	}
}

async fn join_resp(master_url: &str, server_id: &str, name: &str) -> MenuResponse {
	match join(master_url, server_id, name).await {
		Ok((transport, token)) => MenuResponse::Connect {
			transport: Box::new(transport),
			token,
		},
		Err(reason) => MenuResponse::JoinFailed { reason },
	}
}

async fn fetch_servers(master_url: &str) -> Result<Vec<ServerEntry>, String> {
	let text = fetch_text("GET", &format!("{master_url}/api/servers"), None).await?;
	let list: ListResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
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

async fn join(
	master_url: &str,
	server_id: &str,
	name: &str,
) -> Result<(digblock_net_web::WebTransportClient, [u8; 16]), String> {
	let body = serde_json::json!({ "serverId": server_id, "displayName": name }).to_string();
	let text = fetch_text("POST", &format!("{master_url}/api/join"), Some(body))
		.await
		.map_err(|e| format!("join request failed: {e}"))?;
	let join: JoinResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
	let token = parse_token(&join.token)?;
	let hash = digblock_net_web::parse_sha256_hex(&join.cert_hash)
		.map_err(|e| format!("bad certHash from master: {e}"))?;
	Ok((digblock_net_web::connect(&join.url, hash), token))
}

fn parse_token(hex: &str) -> Result<[u8; 16], String> {
	if hex.len() != 32 {
		return Err(format!("bad token length {}", hex.len()));
	}
	let mut token = [0u8; 16];
	for (i, byte) in token.iter_mut().enumerate() {
		*byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
	}
	Ok(token)
}

async fn fetch_text(method: &str, url: &str, body: Option<String>) -> Result<String, String> {
	let init = web_sys::RequestInit::new();
	init.set_method(method);
	if let Some(b) = &body {
		init.set_body(&JsValue::from_str(b));
	}
	let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(|e| js_err(&e))?;
	if body.is_some() {
		request
			.headers()
			.set("content-type", "application/json")
			.map_err(|e| js_err(&e))?;
	}
	let window = web_sys::window().ok_or("no window")?;
	let resp = JsFuture::from(window.fetch_with_request(&request))
		.await
		.map_err(|e| js_err(&e))?;
	let resp: web_sys::Response = resp.unchecked_into();
	let text = JsFuture::from(resp.text().map_err(|e| js_err(&e))?)
		.await
		.map_err(|e| js_err(&e))?;
	let text = text.as_string().unwrap_or_default();
	if !resp.ok() {
		return Err(format!("HTTP {}: {text}", resp.status()));
	}
	Ok(text)
}

fn js_err(v: &JsValue) -> String {
	v.as_string().unwrap_or_else(|| format!("{v:?}"))
}

async fn sleep_ms(ms: i32) {
	let p = js_sys::Promise::new(&mut |resolve, _| {
		web_sys::window()
			.expect("window")
			.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
			.expect("setTimeout");
	});
	let _ = JsFuture::from(p).await;
}
