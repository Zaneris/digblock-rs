//! Startup/pause menu: a pure state machine plus pixel layout — no I/O, no
//! GPU, fully unit-testable. The launcher owns HTTP and ping probing on a
//! service thread and talks over the `MenuServices` channels; the engine
//! routes input here while the menu is open and renders the `MenuView`
//! through the overlay. The world keeps running (and rendering) behind it.

use digblock_net::transport::NetTransport;
use digblock_platform::{InputEvent, Key, MouseButton};

/// One server from the master's list (`GET /api/servers`).
pub struct ServerEntry {
	pub id: String,
	pub name: String,
	pub url: String,
	/// 64-hex SHA-256 pin for the server's TLS cert.
	pub cert_hash: String,
	pub players: u32,
	pub max_players: u32,
}

/// Menu → launcher requests, serviced on the launcher's thread.
pub enum MenuCommand {
	RefreshServers,
	/// Mint a one-time token for `server_id` as `name`, connect, and send
	/// back `MenuResponse::Connect`.
	Join {
		server_id: String,
		name: String,
	},
}

/// Launcher → menu results.
pub enum MenuResponse {
	ServerList(Result<Vec<ServerEntry>, String>),
	/// Client-side reachability probe result; None = failed/timed out.
	Ping {
		server_id: String,
		ms: Option<u32>,
	},
	/// Ready-to-use transport plus the minted token for Hello.
	Connect {
		transport: Box<dyn NetTransport>,
		token: [u8; 16],
	},
	JoinFailed {
		reason: String,
	},
}

/// The channel pair the launcher hands the engine. Keeps HTTP entirely out
/// of the engine (wasm-clean) and off the main thread.
pub struct MenuServices {
	pub cmd_tx: std::sync::mpsc::Sender<MenuCommand>,
	pub resp_rx: std::sync::mpsc::Receiver<MenuResponse>,
}

enum PingState {
	Pending,
	Failed,
	Ms(u32),
}

struct BrowserRow {
	entry: ServerEntry,
	ping: PingState,
}

struct BrowserState {
	rows: Vec<BrowserRow>,
	sel: usize,
	status: Option<String>,
	fetching: bool,
}

enum Screen {
	Root {
		sel: usize,
	},
	Browser(BrowserState),
	/// Keeps the browser state so cancel/failure restores it intact.
	Connecting {
		prev: BrowserState,
		label: String,
	},
}

#[derive(Clone, Copy)]
enum Act {
	CloseMenu,
	OpenMultiplayer,
	LeaveServer,
	JoinRow(usize),
	Refresh,
}

/// Stable identity linking selectable actions to laid-out rows, so
/// hit-testing and rendering can't drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowId {
	RootItem(u8),
	Server(usize),
	Refresh,
}

pub struct Menu {
	/// Config gate: false (bench/screenshot/drill/iOS) = never opens.
	enabled: bool,
	/// A master URL is configured and the launcher wired services.
	services_available: bool,
	screen: Option<Screen>,
	/// Player display name, edited in the browser's name field.
	name: String,
	/// Engine-maintained: currently connected to a server?
	connected: bool,
	pending_cmds: Vec<MenuCommand>,
	wants_disconnect: bool,
	cursor: (f32, f32),
}

impl Menu {
	pub fn new(enabled: bool, name: String) -> Self {
		Self {
			enabled,
			services_available: false,
			screen: enabled.then_some(Screen::Root { sel: 0 }),
			name,
			connected: false,
			pending_cmds: Vec::new(),
			wants_disconnect: false,
			cursor: (-1.0, -1.0),
		}
	}

	pub fn set_services_available(&mut self) {
		self.services_available = true;
	}

	pub fn is_open(&self) -> bool {
		self.screen.is_some()
	}

	/// In-game Escape: reopen at the root (no-op unless enabled).
	pub fn open(&mut self) {
		if self.enabled && self.screen.is_none() {
			self.screen = Some(Screen::Root { sel: 0 });
		}
	}

	pub fn set_connected(&mut self, connected: bool) {
		self.connected = connected;
	}

	pub fn take_commands(&mut self) -> Vec<MenuCommand> {
		std::mem::take(&mut self.pending_cmds)
	}

	pub fn take_disconnect(&mut self) -> bool {
		std::mem::take(&mut self.wants_disconnect)
	}

	// ---- engine hooks -----------------------------------------------------

	/// Welcome landed: the join finished, close the menu.
	pub fn on_welcome(&mut self) {
		if matches!(self.screen, Some(Screen::Connecting { .. })) {
			self.screen = None;
		}
	}

	/// Refused/Disconnected while a join was in flight: back to the browser
	/// with the reason on the status line.
	pub fn on_net_failed(&mut self, reason: &str) {
		if matches!(self.screen, Some(Screen::Connecting { .. })) {
			let Some(Screen::Connecting { mut prev, .. }) = self.screen.take() else {
				return;
			};
			prev.status = Some(reason.to_string());
			self.screen = Some(Screen::Browser(prev));
		}
	}

	/// Feed one service response in; a `Connect` hands the transport+token
	/// back for the engine to install.
	pub fn on_response(&mut self, resp: MenuResponse) -> Option<(Box<dyn NetTransport>, [u8; 16])> {
		match resp {
			MenuResponse::ServerList(result) => {
				if let Some(Screen::Browser(b)) = &mut self.screen {
					match result {
						Ok(list) => {
							b.rows = list
								.into_iter()
								.map(|entry| BrowserRow {
									entry,
									ping: PingState::Pending,
								})
								.collect();
							b.status = None;
						}
						Err(e) => b.status = Some(format!("server list failed: {e}")),
					}
					b.fetching = false;
					b.sel = 0;
				}
			}
			MenuResponse::Ping { server_id, ms } => {
				if let Some(Screen::Browser(b)) = &mut self.screen {
					if let Some(row) = b.rows.iter_mut().find(|r| r.entry.id == server_id) {
						row.ping = match ms {
							Some(ms) => PingState::Ms(ms),
							None => PingState::Failed,
						};
					}
				}
			}
			MenuResponse::Connect { transport, token } => {
				if let Some(Screen::Connecting { label, .. }) = &mut self.screen {
					*label = "Joining world...".into();
					return Some((transport, token));
				}
				// User already cancelled: dropping the transport closes it.
			}
			MenuResponse::JoinFailed { reason } => self.on_net_failed(&reason),
		}
		None
	}

	// ---- input ------------------------------------------------------------

	pub fn handle_input(&mut self, event: &InputEvent, size: (u32, u32)) {
		match *event {
			InputEvent::Key { key, pressed } if pressed => self.handle_key(key),
			InputEvent::Text { ch } => self.handle_text(ch),
			InputEvent::CursorMoved { x, y } => {
				self.cursor = (x, y);
				// Hover moves the keyboard selection (single highlight).
				if let Some(i) = self.act_index_at(x, y, size) {
					self.set_sel(i);
				}
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			} => {
				if let Some(i) = self.act_index_at(self.cursor.0, self.cursor.1, size) {
					let act = self.acts()[i].1;
					self.activate(act);
				}
			}
			_ => {}
		}
	}

	fn current_sel(&self) -> usize {
		match &self.screen {
			Some(Screen::Root { sel }) => *sel,
			Some(Screen::Browser(b)) => b.sel,
			_ => 0,
		}
	}

	fn set_sel(&mut self, sel: usize) {
		match &mut self.screen {
			Some(Screen::Root { sel: s }) => *s = sel,
			Some(Screen::Browser(b)) => b.sel = sel,
			_ => {}
		}
	}

	fn handle_key(&mut self, key: Key) {
		let sel_max = self.acts().len().saturating_sub(1);
		match key {
			Key::Up => self.set_sel(self.current_sel().saturating_sub(1)),
			Key::Down => self.set_sel((self.current_sel() + 1).min(sel_max)),
			Key::Enter => {
				if let Some(&(_, act)) = self.acts().get(self.current_sel()) {
					self.activate(act);
				}
			}
			Key::Backspace => {
				if let Some(Screen::Browser(b)) = &mut self.screen {
					b.status = None;
					self.name.pop();
				}
			}
			Key::Escape => match &self.screen {
				Some(Screen::Root { .. }) => self.screen = None,
				Some(Screen::Browser(_)) => self.screen = Some(Screen::Root { sel: 0 }),
				Some(Screen::Connecting { .. }) => self.cancel_connect(),
				None => {}
			},
			_ => {}
		}
	}

	fn handle_text(&mut self, ch: char) {
		if let Some(Screen::Browser(b)) = &mut self.screen {
			b.status = None;
			if self.name.len() < digblock_net::protocol::MAX_NAME_LEN && !ch.is_control() {
				self.name.push(ch);
			}
		}
	}

	fn cancel_connect(&mut self) {
		// Dropping the pending client closes any in-flight transport.
		self.wants_disconnect = true;
		if let Some(Screen::Connecting { mut prev, .. }) = self.screen.take() {
			prev.status = Some("join cancelled".into());
			self.screen = Some(Screen::Browser(prev));
		}
	}

	fn activate(&mut self, act: Act) {
		match act {
			Act::CloseMenu => self.screen = None,
			Act::OpenMultiplayer => {
				self.pending_cmds.push(MenuCommand::RefreshServers);
				self.screen = Some(Screen::Browser(BrowserState {
					rows: Vec::new(),
					sel: 0,
					status: None,
					fetching: true,
				}));
			}
			Act::LeaveServer => {
				self.wants_disconnect = true;
				self.screen = None;
			}
			Act::Refresh => {
				if let Some(Screen::Browser(b)) = &mut self.screen {
					if !b.fetching {
						b.fetching = true;
						b.status = None;
						self.pending_cmds.push(MenuCommand::RefreshServers);
					}
				}
			}
			Act::JoinRow(i) => {
				let name = self.name.trim().to_string();
				if name.is_empty() {
					if let Some(Screen::Browser(b)) = &mut self.screen {
						b.status = Some("type a name first".into());
					}
					return;
				}
				let Some(Screen::Browser(prev)) = self.screen.take() else {
					return;
				};
				let Some((server_id, server_name)) = prev
					.rows
					.get(i)
					.map(|r| (r.entry.id.clone(), r.entry.name.clone()))
				else {
					self.screen = Some(Screen::Browser(prev));
					return;
				};
				self.pending_cmds
					.push(MenuCommand::Join { server_id, name });
				self.screen = Some(Screen::Connecting {
					prev,
					label: format!("Connecting to {server_name}..."),
				});
			}
		}
	}

	// ---- layout / view ----------------------------------------------------

	/// Ordered selectable actions for the current screen; the selection
	/// index and hit-testing both index into this.
	fn acts(&self) -> Vec<(RowId, Act)> {
		let mut acts = Vec::new();
		match &self.screen {
			Some(Screen::Root { .. }) => {
				acts.push((RowId::RootItem(0), Act::CloseMenu));
				if self.services_available {
					acts.push((RowId::RootItem(1), Act::OpenMultiplayer));
				}
				if self.connected {
					acts.push((RowId::RootItem(2), Act::LeaveServer));
				}
			}
			Some(Screen::Browser(b)) => {
				for i in 0..b.rows.len() {
					acts.push((RowId::Server(i), Act::JoinRow(i)));
				}
				acts.push((RowId::Refresh, Act::Refresh));
			}
			_ => {}
		}
		acts
	}

	fn act_index_at(&self, x: f32, y: f32, size: (u32, u32)) -> Option<usize> {
		let view = self.view(size, false);
		self.acts().iter().position(|(row_id, _)| {
			view.rows
				.iter()
				.find(|r| r.id == Some(*row_id))
				.is_some_and(|row| {
					let [rx, ry, rw, rh] = row.rect;
					x >= rx && x < rx + rw && y >= ry && y < ry + rh
				})
		})
	}

	/// Build the frame's view. `blink` drives the name-field caret.
	pub fn view(&self, size: (u32, u32), blink: bool) -> MenuView {
		const ROW_H: f32 = 28.0;
		const PANEL_W: f32 = 560.0;
		let (w, h) = (size.0 as f32, size.1 as f32);
		let pw = PANEL_W.min(w - 16.0);
		let px = ((w - pw) / 2.0).max(8.0);
		let y0 = (h * 0.18).max(8.0);
		let mut y = y0;
		let mut rows: Vec<PlacedRow> = Vec::new();
		let selected_id = self.acts().get(self.current_sel()).map(|a| a.0);
		let push = |rows: &mut Vec<PlacedRow>, y: &mut f32, spec: RowSpec| {
			let rect = [px, *y, pw, ROW_H];
			*y += ROW_H;
			rows.push(PlacedRow {
				rect,
				id: spec.id,
				text: spec.text,
				right: spec.right,
				color: spec.color,
				highlight: spec.id.is_some() && spec.id == selected_id,
			});
		};

		match &self.screen {
			None => {}
			Some(Screen::Root { .. }) => {
				push(&mut rows, &mut y, RowSpec::title("DIGBLOCK"));
				y += ROW_H * 0.5;
				push(
					&mut rows,
					&mut y,
					RowSpec::item(
						RowId::RootItem(0),
						if self.connected {
							"Back to Game"
						} else {
							"Play Local"
						},
					),
				);
				if self.services_available {
					push(
						&mut rows,
						&mut y,
						RowSpec::item(RowId::RootItem(1), "Multiplayer"),
					);
				} else {
					push(
						&mut rows,
						&mut y,
						RowSpec::dim("Multiplayer (no master server; run with --master-url)"),
					);
				}
				if self.connected {
					push(
						&mut rows,
						&mut y,
						RowSpec::item(RowId::RootItem(2), "Leave Server"),
					);
				}
				y += ROW_H * 0.5;
				push(
					&mut rows,
					&mut y,
					RowSpec::dim("arrows + enter, esc: back to game"),
				);
			}
			Some(Screen::Browser(b)) => {
				push(&mut rows, &mut y, RowSpec::title("MULTIPLAYER"));
				let caret = if blink { "_" } else { " " };
				push(
					&mut rows,
					&mut y,
					RowSpec::field(format!("name: {}{caret}", self.name)),
				);
				y += ROW_H * 0.25;
				if b.fetching && b.rows.is_empty() {
					push(&mut rows, &mut y, RowSpec::dim("fetching server list..."));
				} else if b.rows.is_empty() {
					push(&mut rows, &mut y, RowSpec::dim("no servers online"));
				}
				for (i, row) in b.rows.iter().enumerate() {
					let (ping, color) = match row.ping {
						PingState::Pending => ("...".to_string(), C_DIM),
						PingState::Failed => ("n/a".to_string(), C_RED),
						PingState::Ms(ms) => (
							format!("{ms} ms"),
							if ms < 60 {
								C_GREEN
							} else if ms < 150 {
								C_YELLOW
							} else {
								C_RED
							},
						),
					};
					push(
						&mut rows,
						&mut y,
						RowSpec {
							id: Some(RowId::Server(i)),
							text: format!(
								"{}  {}/{}",
								row.entry.name, row.entry.players, row.entry.max_players
							),
							right: Some((ping, color)),
							color: C_WHITE,
						},
					);
				}
				push(&mut rows, &mut y, RowSpec::item(RowId::Refresh, "Refresh"));
				if let Some(status) = &b.status {
					push(&mut rows, &mut y, RowSpec::status(status.clone()));
				}
				y += ROW_H * 0.25;
				push(
					&mut rows,
					&mut y,
					RowSpec::dim("type your name, enter joins, esc: back"),
				);
			}
			Some(Screen::Connecting { label, .. }) => {
				push(&mut rows, &mut y, RowSpec::title("MULTIPLAYER"));
				y += ROW_H * 0.5;
				push(&mut rows, &mut y, RowSpec::dim(label.clone()));
				push(&mut rows, &mut y, RowSpec::dim("esc: cancel"));
			}
		}

		MenuView {
			panel: [px - 12.0, y0 - 12.0, pw + 24.0, (y - y0) + 24.0],
			rows,
		}
	}
}

// Overlay color format (0xAABBGGRR, matching overlay.rs).
const C_WHITE: u32 = 0xFFFFFFFF;
const C_DIM: u32 = 0xFFB0B0B0;
const C_GREEN: u32 = 0xFF40E070;
const C_YELLOW: u32 = 0xFF30D0F0;
const C_RED: u32 = 0xFF4040F0;

struct RowSpec {
	id: Option<RowId>,
	text: String,
	right: Option<(String, u32)>,
	color: u32,
}

impl RowSpec {
	fn title(t: &str) -> Self {
		Self {
			id: None,
			text: t.into(),
			right: None,
			color: C_WHITE,
		}
	}
	fn item(id: RowId, t: &str) -> Self {
		Self {
			id: Some(id),
			text: t.into(),
			right: None,
			color: C_WHITE,
		}
	}
	fn field(t: String) -> Self {
		Self {
			id: None,
			text: t,
			right: None,
			color: C_WHITE,
		}
	}
	fn status(t: String) -> Self {
		Self {
			id: None,
			text: t,
			right: None,
			color: C_RED,
		}
	}
	fn dim(t: impl Into<String>) -> Self {
		Self {
			id: None,
			text: t.into(),
			right: None,
			color: C_DIM,
		}
	}
}

/// One laid-out menu row for the overlay.
pub struct PlacedRow {
	pub rect: [f32; 4],
	pub id: Option<RowId>,
	pub text: String,
	/// Right-aligned colored text (the ping column).
	pub right: Option<(String, u32)>,
	pub color: u32,
	pub highlight: bool,
}

pub struct MenuView {
	/// Backdrop rect enclosing all rows.
	pub panel: [f32; 4],
	pub rows: Vec<PlacedRow>,
}

#[cfg(test)]
mod tests {
	use super::*;

	const SIZE: (u32, u32) = (1280, 800);

	fn entry(id: &str, name: &str) -> ServerEntry {
		ServerEntry {
			id: id.into(),
			name: name.into(),
			url: format!("https://{id}:4433"),
			cert_hash: "aa".repeat(32),
			players: 1,
			max_players: 255,
		}
	}

	fn open_browser(menu: &mut Menu) {
		menu.handle_key(Key::Down); // Multiplayer
		menu.handle_key(Key::Enter);
	}

	#[test]
	fn disabled_menu_never_opens() {
		let mut menu = Menu::new(false, "ted".into());
		assert!(!menu.is_open());
		menu.open();
		assert!(!menu.is_open());
	}

	#[test]
	fn root_play_local_closes_and_escape_reopens() {
		let mut menu = Menu::new(true, "ted".into());
		assert!(menu.is_open());
		menu.handle_key(Key::Enter); // Play Local
		assert!(!menu.is_open());
		menu.open();
		assert!(menu.is_open());
	}

	#[test]
	fn multiplayer_requires_services() {
		let mut menu = Menu::new(true, "ted".into());
		// Without services the only act is Play Local; Down stays there.
		assert_eq!(menu.acts().len(), 1);
		menu.set_services_available();
		assert_eq!(menu.acts().len(), 2);
	}

	#[test]
	fn browser_flow_join_and_fail_returns_with_status() {
		let mut menu = Menu::new(true, "ted".into());
		menu.set_services_available();
		open_browser(&mut menu);
		assert!(matches!(
			menu.take_commands().as_slice(),
			[MenuCommand::RefreshServers]
		));
		menu.on_response(MenuResponse::ServerList(Ok(vec![
			entry("s1", "Alpha"),
			entry("s2", "Beta"),
		])));
		menu.on_response(MenuResponse::Ping {
			server_id: "s2".into(),
			ms: Some(34),
		});
		let view = menu.view(SIZE, false);
		assert!(view.rows.iter().any(|r| r.text.contains("Alpha")));
		assert!(view
			.rows
			.iter()
			.any(|r| matches!(&r.right, Some((p, _)) if p == "34 ms")));

		// Join the second server.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		match menu.take_commands().as_slice() {
			[MenuCommand::Join { server_id, name }] => {
				assert_eq!(server_id, "s2");
				assert_eq!(name, "ted");
			}
			other => panic!("expected Join, got {} commands", other.len()),
		}
		// Failure lands back in the browser with the reason shown.
		menu.on_net_failed("token rejected");
		let view = menu.view(SIZE, false);
		assert!(view.rows.iter().any(|r| r.text.contains("token rejected")));
		assert!(
			view.rows.iter().any(|r| r.text.contains("Beta")),
			"rows kept"
		);
	}

	#[test]
	fn empty_name_blocks_join() {
		let mut menu = Menu::new(true, String::new());
		menu.set_services_available();
		open_browser(&mut menu);
		let _ = menu.take_commands(); // drain the RefreshServers from opening
		menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
		menu.handle_key(Key::Enter);
		assert!(menu.take_commands().is_empty());
		let view = menu.view(SIZE, false);
		assert!(view.rows.iter().any(|r| r.text.contains("type a name")));
	}

	#[test]
	fn text_edits_name_with_cap_and_backspace() {
		let mut menu = Menu::new(true, String::new());
		menu.set_services_available();
		open_browser(&mut menu);
		for ch in "ted!".chars() {
			menu.handle_text(ch);
		}
		menu.handle_key(Key::Backspace);
		assert!(menu
			.view(SIZE, false)
			.rows
			.iter()
			.any(|r| r.text.contains("name: ted")));
		for _ in 0..100 {
			menu.handle_text('x');
		}
		let long = menu.view(SIZE, true);
		let name_row = long
			.rows
			.iter()
			.find(|r| r.text.starts_with("name:"))
			.unwrap();
		assert!(name_row.text.len() <= "name: ".len() + digblock_net::protocol::MAX_NAME_LEN + 1);
	}

	#[test]
	fn welcome_closes_menu_and_cancel_restores_browser() {
		let mut menu = Menu::new(true, "ted".into());
		menu.set_services_available();
		open_browser(&mut menu);
		menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
		menu.handle_key(Key::Enter);
		menu.on_welcome();
		assert!(!menu.is_open());

		// Second run: cancel instead.
		menu.open();
		menu.handle_key(Key::Down); // Multiplayer now that connected=false... still item 1
		menu.handle_key(Key::Enter);
		menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
		menu.handle_key(Key::Enter);
		let _ = menu.take_commands();
		menu.handle_key(Key::Escape); // cancel connect
		assert!(menu.take_disconnect());
		assert!(menu
			.view(SIZE, false)
			.rows
			.iter()
			.any(|r| r.text.contains("cancelled")));
	}

	#[test]
	fn click_hits_rows() {
		let mut menu = Menu::new(true, "ted".into());
		menu.set_services_available();
		// Click "Multiplayer" (second selectable row).
		let view = menu.view(SIZE, false);
		let mp_row = view
			.rows
			.iter()
			.find(|r| r.id == Some(RowId::RootItem(1)))
			.unwrap();
		let (cx, cy) = (mp_row.rect[0] + 5.0, mp_row.rect[1] + 5.0);
		menu.handle_input(&InputEvent::CursorMoved { x: cx, y: cy }, SIZE);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
		);
		assert!(matches!(
			menu.take_commands().as_slice(),
			[MenuCommand::RefreshServers]
		));
	}

	#[test]
	fn leave_server_appears_when_connected() {
		let mut menu = Menu::new(true, "ted".into());
		menu.set_connected(true);
		let acts = menu.acts();
		assert_eq!(acts.len(), 2); // Back to Game + Leave Server (no services)
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		assert!(menu.take_disconnect());
		assert!(!menu.is_open());
	}
}
