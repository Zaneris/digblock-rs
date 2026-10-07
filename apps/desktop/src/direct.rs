//! Multiplayer without the master, desktop side: `digblock_client::direct` answers the menu, this
//! keeps its identity in a file beside the other saves and owns the transport. The one server has
//! a CA-issued certificate, validated against the system's roots (`CertPolicy::SystemRoots`).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use digblock_client::direct::{DirectIdentity, DirectStep, server_entry, step};
use digblock_client::{MenuCommand, MenuResponse, MenuServices};
use digblock_net::protocol::DIRECT_SERVER_URL;
use digblock_net_native::{CertPolicy, ChannelTransport};

#[derive(serde::Serialize, serde::Deserialize)]
struct Stored {
	key: String,
	#[serde(default)]
	name: Option<String>,
}

fn path() -> PathBuf {
	std::env::var("DIG_DIRECT_FILE")
		.map(PathBuf::from)
		.unwrap_or_else(|_| {
			dirs::data_dir()
				.map(|d| d.join("digblock").join("direct.json"))
				.unwrap_or_else(|| PathBuf::from("./digblock-direct.json"))
		})
}

/// This machine's player, or a new one stored for next time, named after its master account
/// when it has no name yet.
pub(crate) fn load_or_create(master_name: Option<String>) -> DirectIdentity {
	let path = path();
	let stored = std::fs::read(&path)
		.ok()
		.and_then(|b| serde_json::from_slice::<Stored>(&b).ok())
		.and_then(|s| DirectIdentity::from_stored(&s.key, s.name));
	let fresh = stored.is_none();
	let mut me = stored.unwrap_or_else(|| DirectIdentity::new(rand::random()));
	if me.adopt_name(master_name) || fresh {
		store(&path, &me);
	}
	me
}

/// Owner-readable only: the key is the character, on the one server that knows it.
fn store(path: &Path, me: &DirectIdentity) {
	let write = || -> std::io::Result<()> {
		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir)?;
		}
		let stored = Stored {
			key: me.key_hex(),
			name: me.name.clone(),
		};
		std::fs::write(path, serde_json::to_vec_pretty(&stored)?)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
		}
		Ok(())
	};
	if let Err(e) = write() {
		log::warn!(
			"could not save the player key to {} ({e}); next launch is a new player",
			path.display()
		);
	}
}

/// Join the one server straight away: `--join` without a master.
pub(crate) fn connect(me: &DirectIdentity) -> (ChannelTransport, [u8; 16]) {
	let transport = digblock_net_native::connect_with(DIRECT_SERVER_URL, CertPolicy::SystemRoots);
	(transport, me.token())
}

pub(crate) fn start(mut me: DirectIdentity) -> MenuServices {
	let (cmd_tx, cmd_rx) = mpsc::channel::<MenuCommand>();
	let (resp_tx, resp_rx) = mpsc::channel::<MenuResponse>();
	let _ = resp_tx.send(MenuResponse::Account(Some(me.view())));
	let spawned = std::thread::Builder::new()
		.name("menu-direct".into())
		.spawn(move || {
			let path = path();
			for cmd in cmd_rx {
				let resp = match step(&mut me, cmd) {
					DirectStep::Reply(resp) => resp,
					DirectStep::Renamed(resp) => {
						store(&path, &me);
						resp
					}
					DirectStep::List => {
						let _ = resp_tx.send(MenuResponse::ServerList(Ok(vec![server_entry()])));
						let resp_tx = resp_tx.clone();
						digblock_net_native::probe(
							DIRECT_SERVER_URL,
							CertPolicy::SystemRoots,
							Duration::from_secs(3),
							move |ping| {
								let _ = resp_tx.send(MenuResponse::Ping {
									server_id: DIRECT_SERVER_URL.into(),
									ping,
								});
							},
						);
						continue;
					}
					DirectStep::Join => {
						let (transport, token) = connect(&me);
						MenuResponse::Connect {
							transport: Box::new(transport),
							token,
						}
					}
					DirectStep::Nothing => continue,
				};
				if resp_tx.send(resp).is_err() {
					return;
				}
			}
		});
	if let Err(e) = spawned {
		log::error!("could not start the multiplayer service: {e}");
	}
	MenuServices { cmd_tx, resp_rx }
}
