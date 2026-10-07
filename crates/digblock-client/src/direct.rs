//! Multiplayer without the master, the part both launchers share: the device identity, the one
//! server's listing, and the menu commands answered locally. A launcher keeps the identity in its
//! own storage and owns the transport; everything it would otherwise have to agree on with the
//! other launcher lives here. The account shown is the one the server derives from the key
//! (`protocol::direct_account`), which is what an operator lists as admin.

use crate::menu::{AccountView, MenuCommand, MenuResponse, ServerEntry};
use digblock_net::protocol::{
	DIRECT_SERVER_NAME, DIRECT_SERVER_URL, PROTOCOL_VERSION, direct_account, direct_token,
	validate_name,
};

/// Why there is no board; the hiscores screen shows it as "Hiscores failed: ...".
const NO_BOARD: &str = "offline for now";

/// One device's player: a random key, and the name chosen for it.
#[derive(Clone)]
pub struct DirectIdentity {
	key: [u8; 32],
	pub name: Option<String>,
}

impl DirectIdentity {
	/// A fresh player from 32 bytes of the platform's secure randomness.
	pub fn new(key: [u8; 32]) -> Self {
		Self { key, name: None }
	}

	/// A stored player: the key as [`DirectIdentity::key_hex`] wrote it. `None` for anything else.
	pub fn from_stored(key_hex: &str, name: Option<String>) -> Option<Self> {
		if key_hex.len() != 64 || !key_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
			return None;
		}
		let mut key = [0u8; 32];
		for (i, byte) in key.iter_mut().enumerate() {
			*byte = u8::from_str_radix(&key_hex[i * 2..i * 2 + 2], 16).ok()?;
		}
		Some(Self { key, name })
	}

	pub fn key_hex(&self) -> String {
		self.key.iter().map(|b| format!("{b:02x}")).collect()
	}

	/// A player with no name yet takes the one their master account had, so a returning player
	/// is not asked again. True when it did, and the launcher should store the identity.
	pub fn adopt_name(&mut self, master_name: Option<String>) -> bool {
		let Some(name) = master_name.filter(|n| validate_name(n).is_ok()) else {
			return false;
		};
		if self.name.is_some() {
			return false;
		}
		self.name = Some(name);
		true
	}

	/// The Hello token for the one server.
	pub fn token(&self) -> [u8; 16] {
		direct_token(&self.key, DIRECT_SERVER_URL)
	}

	pub fn view(&self) -> AccountView {
		AccountView {
			id: direct_account(&self.token()).to_string(),
			name: self.name.clone(),
		}
	}
}

/// The one server, as the browser lists it.
pub fn server_entry() -> ServerEntry {
	ServerEntry {
		id: DIRECT_SERVER_URL.into(),
		name: DIRECT_SERVER_NAME.into(),
		url: DIRECT_SERVER_URL.into(),
		cert_hash: String::new(),
		// Nobody counts players without a master; zero hides the column.
		players: 0,
		max_players: 0,
		// The handshake is the real check, and says why if it refuses.
		protocol_version: PROTOCOL_VERSION,
		sim_version: digblock_core::SIM_VERSION,
		reports_skills: false,
	}
}

/// What a launcher does with one menu command.
pub enum DirectStep {
	/// Send this, nothing more.
	Reply(MenuResponse),
	/// The name changed: store the identity, then send this.
	Renamed(MenuResponse),
	/// Send `ServerList` of [`server_entry`], then probe it over the launcher's transport.
	List,
	/// Connect to [`DIRECT_SERVER_URL`] and hand over the transport with [`DirectIdentity::token`].
	Join,
	Nothing,
}

/// The menu's command, answered without a master.
pub fn step(me: &mut DirectIdentity, cmd: MenuCommand) -> DirectStep {
	match cmd {
		MenuCommand::RefreshServers => DirectStep::List,
		MenuCommand::Join { .. } => DirectStep::Join,
		// The key is the account, so there is nothing to sign in to or out
		// of: Sign Out keeps the key, and signing in hands it straight back.
		MenuCommand::SignIn => DirectStep::Reply(MenuResponse::Account(Some(me.view()))),
		MenuCommand::CancelSignIn | MenuCommand::SignOut => DirectStep::Nothing,
		MenuCommand::CheckName { name } => {
			let reason = validate_name(&name).err().map(str::to_string);
			DirectStep::Reply(MenuResponse::NameAvailable {
				available: reason.is_none(),
				name,
				reason,
			})
		}
		MenuCommand::SetName { name } => match validate_name(&name) {
			Ok(()) => {
				me.name = Some(name.clone());
				DirectStep::Renamed(MenuResponse::NameSet(Ok(name)))
			}
			Err(e) => DirectStep::Reply(MenuResponse::NameSet(Err(e.into()))),
		},
		MenuCommand::FetchRanks => DirectStep::Reply(MenuResponse::Ranks(Err(NO_BOARD.into()))),
		MenuCommand::FetchBoard { .. } => {
			DirectStep::Reply(MenuResponse::Board(Err(NO_BOARD.into())))
		}
	}
}

#[cfg(test)]
mod tests;
