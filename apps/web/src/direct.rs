//! Multiplayer without the master, browser side: `digblock_client::direct` answers the menu, this
//! keeps its identity in localStorage and owns the transport. The one server has a CA-issued
//! certificate, so the browser validates it like any https site and no pin is needed.

use std::sync::mpsc;

use digblock_client::direct::{DirectIdentity, DirectStep, server_entry, step};
use digblock_client::{MenuCommand, MenuResponse, MenuServices};
use digblock_net::protocol::DIRECT_SERVER_URL;

const STORE_KEY: &str = "digblock.direct";

#[derive(serde::Serialize, serde::Deserialize)]
struct Stored {
	key: String,
	#[serde(default)]
	name: Option<String>,
}

/// This browser's player, or a new one stored for next time, named after its master account
/// when it has no name yet. `None` only when the browser has no secure randomness: a guessable
/// key would hand the character to anyone.
pub(crate) fn load_or_create(master_name: Option<String>) -> Option<DirectIdentity> {
	let stored = local()
		.and_then(|s| s.get_item(STORE_KEY).ok().flatten())
		.and_then(|raw| serde_json::from_str::<Stored>(&raw).ok())
		.and_then(|s| DirectIdentity::from_stored(&s.key, s.name));
	let (mut me, fresh) = match stored {
		Some(me) => (me, false),
		None => {
			let mut key = [0u8; 32];
			web_sys::window()
				.and_then(|w| w.crypto().ok())
				.and_then(|c| c.get_random_values_with_u8_array(&mut key).ok())?;
			(DirectIdentity::new(key), true)
		}
	};
	if me.adopt_name(master_name) || fresh {
		store(&me);
	}
	Some(me)
}

/// Best effort: a browser refusing storage still plays, as a new player next visit.
fn store(me: &DirectIdentity) {
	let stored = Stored {
		key: me.key_hex(),
		name: me.name.clone(),
	};
	match (local(), serde_json::to_string(&stored)) {
		(Some(s), Ok(json)) if s.set_item(STORE_KEY, &json).is_ok() => {}
		_ => log::warn!("could not save this browser's player key; next visit is a new player"),
	}
}

fn local() -> Option<web_sys::Storage> {
	web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

pub(crate) fn start(me: DirectIdentity, no_datagrams: bool) -> MenuServices {
	let (cmd_tx, cmd_rx) = mpsc::channel::<MenuCommand>();
	let (resp_tx, resp_rx) = mpsc::channel::<MenuResponse>();
	wasm_bindgen_futures::spawn_local(run(me, no_datagrams, cmd_rx, resp_tx));
	MenuServices { cmd_tx, resp_rx }
}

async fn run(
	mut me: DirectIdentity,
	no_datagrams: bool,
	cmd_rx: mpsc::Receiver<MenuCommand>,
	resp_tx: mpsc::Sender<MenuResponse>,
) {
	let _ = resp_tx.send(MenuResponse::Account(Some(me.view())));
	loop {
		while let Ok(cmd) = cmd_rx.try_recv() {
			match step(&mut me, cmd) {
				DirectStep::Reply(resp) => {
					let _ = resp_tx.send(resp);
				}
				DirectStep::Renamed(resp) => {
					store(&me);
					let _ = resp_tx.send(resp);
				}
				DirectStep::List => {
					let _ = resp_tx.send(MenuResponse::ServerList(Ok(vec![server_entry()])));
					let resp_tx = resp_tx.clone();
					digblock_net_web::probe(DIRECT_SERVER_URL, None, 3000, move |ping| {
						let _ = resp_tx.send(MenuResponse::Ping {
							server_id: DIRECT_SERVER_URL.into(),
							ping,
						});
					});
				}
				DirectStep::Join => {
					let transport =
						digblock_net_web::connect(DIRECT_SERVER_URL, None, no_datagrams);
					let _ = resp_tx.send(MenuResponse::Connect {
						transport: Box::new(transport),
						token: me.token(),
					});
				}
				DirectStep::Nothing => {}
			}
		}
		crate::services::sleep_ms(150).await;
	}
}
