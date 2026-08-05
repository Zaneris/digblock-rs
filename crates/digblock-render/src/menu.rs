//! Startup/pause menu: a pure state machine plus pixel layout — no I/O, no
//! GPU, fully unit-testable. The launcher owns HTTP and ping probing on a
//! service thread and talks over the `MenuServices` channels; the engine
//! routes input here while the menu is open and renders the `MenuView`
//! through the overlay. The world keeps running (and rendering) behind it.

use crate::settings::{Settings, DRAW_MAX, DRAW_MIN, DRAW_STEP, VOL_MAX, VOL_STEP};
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

/// Menu → engine intents for named local worlds (take-pattern, not
/// `MenuServices` — the world store is engine-local and synchronous).
pub enum WorldIntent {
	Refresh,
	Create { name: String },
	Enter { id: String },
	Delete { id: String },
}

/// One saved world as the menu shows it. The engine builds these from
/// the store's `WorldMeta` (formatting the last-played label — the menu
/// stays clock-free).
#[derive(Clone)]
pub struct WorldRow {
	pub id: String,
	pub name: String,
	/// Right-column label: last-played age, or an incompatibility note.
	pub detail: String,
	/// False = from an older format/worldgen version: still listed (and
	/// deletable) but not enterable.
	pub compatible: bool,
}

/// Local-world name rules: same charset as display names (untypeable
/// invalid characters), its own length cap.
const WORLD_NAME_MAX: usize = 16;

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

struct WorldsState {
	rows: Vec<WorldRow>,
	sel: usize,
	status: Option<String>,
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
	Settings {
		sel: usize,
	},
	/// Singleplayer world list.
	Worlds(WorldsState),
	/// New-world name entry (keeps the list for Escape).
	CreateWorld {
		prev: WorldsState,
		name: String,
	},
	/// Per-world actions (enter/delete); `idx` into `prev.rows`.
	WorldActions {
		prev: WorldsState,
		idx: usize,
		sel: usize,
	},
	ConfirmDelete {
		prev: WorldsState,
		idx: usize,
		sel: usize,
	},
}

/// One adjustable row on the Settings screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Setting {
	DrawDist,
	MusicVol,
	SfxVol,
	Ssr,
}

#[derive(Clone, Copy)]
enum Act {
	CloseMenu,
	OpenMultiplayer,
	LeaveServer,
	ReturnSpawn,
	JoinRow(usize),
	Refresh,
	OpenSettings,
	SettingRow(Setting),
	CloseSettings,
	OpenWorlds,
	WorldRow(usize),
	OpenCreate,
	ConfirmCreate,
	EnterWorld,
	DeleteWorld,
	ConfirmDeleteWorld,
	BackToWorlds,
}

/// Stable identity linking selectable actions to laid-out rows, so
/// hit-testing and rendering can't drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowId {
	RootItem(u8),
	Server(usize),
	Refresh,
	/// 0..=3 the settings in screen order, 4 = Back.
	SettingsRow(u8),
	/// One saved world in the Singleplayer list.
	World(usize),
	/// The "Create New World" row (list) / the name field (create).
	CreateWorld,
	/// Rows on the actions/confirm screens, top to bottom.
	WorldAction(u8),
}

pub struct Menu {
	/// Config gate: false (bench/screenshot/drill/iOS) = never opens.
	enabled: bool,
	/// A master URL is configured and the launcher wired services.
	services_available: bool,
	/// A world store was injected: Root offers "Singleplayer" (named
	/// saves) instead of the ephemeral "Play Local".
	store_available: bool,
	pending_worlds: Vec<WorldIntent>,
	screen: Option<Screen>,
	/// Player display name, edited in the browser's name field.
	name: String,
	/// Engine-maintained: currently connected to a server?
	connected: bool,
	/// The player has entered the world at least once (picked Play Local
	/// or completed a join). Until then there is no game to go back to:
	/// Return to Spawn is hidden and Escape can't dismiss the menu.
	entered_world: bool,
	/// Engine-maintained: seconds left on the return-to-spawn cooldown
	/// (0 = ready). On cooldown the row renders dim with the countdown.
	spawn_cooldown_secs: u32,
	pending_cmds: Vec<MenuCommand>,
	wants_disconnect: bool,
	wants_return_spawn: bool,
	/// Live settings, edited on the Settings screen. `settings.name`
	/// tracks the last *joined-with* name; the `name` field above is the
	/// live edit buffer.
	settings: Settings,
	/// A settings value changed since the engine last drained it.
	settings_dirty: bool,
	cursor: (f32, f32),
	/// One-shot scripted join (a name filter; empty = first server):
	/// when services arrive, walk the exact user path — open the
	/// browser, pick the matching row, join — so the Connect handoff
	/// needs no special case. Web `?join` / scripted flows.
	auto_join: Option<String>,
}

impl Menu {
	pub fn new(enabled: bool, settings: Settings, auto_join: Option<String>) -> Self {
		Self {
			enabled,
			services_available: false,
			store_available: false,
			pending_worlds: Vec::new(),
			screen: enabled.then_some(Screen::Root { sel: 0 }),
			name: settings.name.clone(),
			connected: false,
			entered_world: false,
			spawn_cooldown_secs: 0,
			pending_cmds: Vec::new(),
			wants_disconnect: false,
			wants_return_spawn: false,
			settings,
			settings_dirty: false,
			cursor: (-1.0, -1.0),
			auto_join,
		}
	}

	pub fn set_services_available(&mut self) {
		self.services_available = true;
		if self.enabled && self.auto_join.is_some() {
			self.activate(Act::OpenMultiplayer);
		}
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
		// Being on a server implies the startup menu is behind us.
		self.entered_world |= connected;
	}

	/// Seconds left on the return-to-spawn cooldown (0 = ready).
	pub fn set_spawn_cooldown(&mut self, secs: u32) {
		self.spawn_cooldown_secs = secs;
	}

	pub fn take_commands(&mut self) -> Vec<MenuCommand> {
		std::mem::take(&mut self.pending_cmds)
	}

	pub fn take_disconnect(&mut self) -> bool {
		std::mem::take(&mut self.wants_disconnect)
	}

	pub fn take_return_spawn(&mut self) -> bool {
		std::mem::take(&mut self.wants_return_spawn)
	}

	/// The full settings snapshot, once per change batch. The engine
	/// applies runtime effects (draw dist, SSR, volume) and persists.
	pub fn take_settings_change(&mut self) -> Option<Settings> {
		std::mem::take(&mut self.settings_dirty).then(|| self.settings.clone())
	}

	/// World-store intents raised since the last drain.
	pub fn take_world_intents(&mut self) -> Vec<WorldIntent> {
		std::mem::take(&mut self.pending_worlds)
	}

	/// A world store was injected: Root offers named saves.
	pub fn set_store_available(&mut self) {
		self.store_available = true;
	}

	/// The store's world list (engine answers a `WorldIntent::Refresh`,
	/// and re-sends after create/delete).
	pub fn set_world_list(&mut self, rows: Vec<WorldRow>) {
		match &mut self.screen {
			Some(Screen::Worlds(w)) => {
				w.sel = w.sel.min(rows.len()); // create row keeps it valid
				w.rows = rows;
			}
			Some(Screen::CreateWorld { prev, .. })
			| Some(Screen::WorldActions { prev, .. })
			| Some(Screen::ConfirmDelete { prev, .. }) => prev.rows = rows,
			_ => {}
		}
	}

	/// A store error for the worlds status line (open failed, quota...).
	pub fn set_worlds_status(&mut self, status: String) {
		match &mut self.screen {
			Some(Screen::Worlds(w)) => w.status = Some(status),
			Some(Screen::CreateWorld { prev, .. })
			| Some(Screen::WorldActions { prev, .. })
			| Some(Screen::ConfirmDelete { prev, .. }) => prev.status = Some(status),
			_ => {}
		}
	}

	/// A local save opened: the menu's job here is done.
	pub fn on_world_entered(&mut self) {
		self.entered_world = true;
		self.screen = None;
	}

	/// Left a server for a fresh backdrop world: back to the locked
	/// startup menu — there is no game to "go back" to until the player
	/// picks a save or a server again.
	pub fn on_left_server(&mut self) {
		self.connected = false;
		self.entered_world = false;
		if self.enabled {
			self.screen = Some(Screen::Root { sel: 0 });
		}
	}

	// ---- engine hooks -----------------------------------------------------

	/// Welcome landed: the join finished, close the menu.
	pub fn on_welcome(&mut self) {
		if matches!(self.screen, Some(Screen::Connecting { .. })) {
			self.entered_world = true;
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
				// Scripted join: pick the row a user would and go.
				if let Some(filter) = self.auto_join.take() {
					let row = match &self.screen {
						Some(Screen::Browser(b)) => b.rows.iter().position(|r| {
							r.entry.name.to_lowercase().contains(&filter.to_lowercase())
						}),
						_ => None,
					};
					match row {
						Some(i) => self.activate(Act::JoinRow(i)),
						None => {
							if let Some(Screen::Browser(b)) = &mut self.screen {
								b.status = Some("auto-join: no matching server".into());
							}
						}
					}
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
					// A click inside a slider's bar sets the value from the
					// click fraction instead of "activating" the row.
					if let Act::SettingRow(s) = act {
						if let Some(frac) = self.bar_frac_at(self.cursor.0, self.cursor.1, size) {
							self.set_setting_frac(s, frac);
							return;
						}
					}
					self.activate(act);
				}
			}
			_ => {}
		}
	}

	/// If (x, y) lands inside a slider row's bar, the 0..=1 fraction along
	/// it. Uses the same placed rects as rendering, so it can't drift.
	fn bar_frac_at(&self, x: f32, y: f32, size: (u32, u32)) -> Option<f32> {
		let view = self.view(size, false);
		let row = view.rows.iter().find(|r| {
			let [rx, ry, rw, rh] = r.rect;
			r.bar.is_some() && x >= rx && x < rx + rw && y >= ry && y < ry + rh
		})?;
		let [bx, _, bw, _] = bar_rect(row.rect);
		(x >= bx && x < bx + bw).then(|| (x - bx) / bw)
	}

	fn current_sel(&self) -> usize {
		match &self.screen {
			Some(Screen::Root { sel }) => *sel,
			Some(Screen::Browser(b)) => b.sel,
			Some(Screen::Settings { sel }) => *sel,
			Some(Screen::Worlds(w)) => w.sel,
			Some(Screen::WorldActions { sel, .. }) => *sel,
			Some(Screen::ConfirmDelete { sel, .. }) => *sel,
			_ => 0,
		}
	}

	fn set_sel(&mut self, sel: usize) {
		match &mut self.screen {
			Some(Screen::Root { sel: s }) => *s = sel,
			Some(Screen::Browser(b)) => b.sel = sel,
			Some(Screen::Settings { sel: s }) => *s = sel,
			Some(Screen::Worlds(w)) => w.sel = sel,
			Some(Screen::WorldActions { sel: s, .. }) => *s = sel,
			Some(Screen::ConfirmDelete { sel: s, .. }) => *s = sel,
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
			Key::Backspace => match &mut self.screen {
				Some(Screen::Browser(b)) => {
					b.status = None;
					self.name.pop();
				}
				Some(Screen::CreateWorld { name, .. }) => {
					name.pop();
				}
				_ => {}
			},
			Key::Left | Key::Right => {
				// Slider adjust on the Settings screen only.
				if matches!(self.screen, Some(Screen::Settings { .. })) {
					if let Some(&(_, Act::SettingRow(s))) = self.acts().get(self.current_sel()) {
						self.adjust_setting(s, if key == Key::Left { -1 } else { 1 });
					}
				}
			}
			Key::Escape => match self.screen.take() {
				// The startup menu can't be dismissed — there's nothing to
				// go back to until the player has picked a world.
				Some(Screen::Root { .. }) if self.entered_world => {}
				Some(s @ Screen::Root { .. }) => self.screen = Some(s),
				Some(Screen::Browser(_)) => self.screen = Some(Screen::Root { sel: 0 }),
				Some(s @ Screen::Connecting { .. }) => {
					self.screen = Some(s);
					self.cancel_connect();
				}
				Some(Screen::Settings { .. }) => self.screen = Some(Screen::Root { sel: 0 }),
				Some(Screen::Worlds(_)) => self.screen = Some(Screen::Root { sel: 0 }),
				Some(Screen::CreateWorld { prev, .. })
				| Some(Screen::WorldActions { prev, .. })
				| Some(Screen::ConfirmDelete { prev, .. }) => {
					self.screen = Some(Screen::Worlds(prev));
				}
				None => {}
			},
			_ => {}
		}
	}

	/// Step one setting by `dir` (±1). Toggles just flip; sliders move by
	/// their step, snapped to the step grid so float drift can't build up.
	fn adjust_setting(&mut self, s: Setting, dir: i32) {
		let st = &mut self.settings;
		let changed = match s {
			Setting::DrawDist => {
				let v = (st.draw_dist + dir * DRAW_STEP).clamp(DRAW_MIN, DRAW_MAX);
				std::mem::replace(&mut st.draw_dist, v) != v
			}
			Setting::MusicVol => {
				let v = snap_vol(st.music_vol, dir);
				(std::mem::replace(&mut st.music_vol, v) - v).abs() > f32::EPSILON
			}
			Setting::SfxVol => {
				let v = snap_vol(st.sfx_vol, dir);
				(std::mem::replace(&mut st.sfx_vol, v) - v).abs() > f32::EPSILON
			}
			Setting::Ssr => {
				st.ssr = !st.ssr;
				true
			}
		};
		self.settings_dirty |= changed;
	}

	/// Set one slider from a 0..=1 bar fraction (mouse click), snapped.
	fn set_setting_frac(&mut self, s: Setting, frac: f32) {
		let frac = frac.clamp(0.0, 1.0);
		let st = &mut self.settings;
		let changed = match s {
			Setting::DrawDist => {
				let span = (DRAW_MAX - DRAW_MIN) as f32;
				let v = DRAW_MIN
					+ ((frac * span / DRAW_STEP as f32).round() as i32 * DRAW_STEP)
						.clamp(0, DRAW_MAX - DRAW_MIN);
				std::mem::replace(&mut st.draw_dist, v) != v
			}
			Setting::MusicVol => {
				let v = (frac * VOL_MAX / VOL_STEP).round() * VOL_STEP;
				(std::mem::replace(&mut st.music_vol, v) - v).abs() > f32::EPSILON
			}
			Setting::SfxVol => {
				let v = (frac * VOL_MAX / VOL_STEP).round() * VOL_STEP;
				(std::mem::replace(&mut st.sfx_vol, v) - v).abs() > f32::EPSILON
			}
			// Not a slider; a bar click can't reach it.
			Setting::Ssr => false,
		};
		self.settings_dirty |= changed;
	}

	fn handle_text(&mut self, ch: char) {
		use digblock_net::protocol::{is_name_char, MAX_DISPLAY_NAME};
		match &mut self.screen {
			Some(Screen::Browser(b)) => {
				b.status = None;
				// Invalid characters (spaces, punctuation, non-ASCII) are
				// untypeable rather than rejected later.
				if self.name.chars().count() < MAX_DISPLAY_NAME && is_name_char(ch) {
					self.name.push(ch);
				}
			}
			// World names: same untypeable-invalid-chars property, own cap.
			Some(Screen::CreateWorld { name, .. }) => {
				if name.chars().count() < WORLD_NAME_MAX && is_name_char(ch) {
					name.push(ch);
				}
			}
			_ => {}
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
			Act::CloseMenu => {
				self.entered_world = true;
				self.screen = None;
			}
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
			Act::ReturnSpawn => {
				self.wants_return_spawn = true;
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
			Act::OpenSettings => self.screen = Some(Screen::Settings { sel: 0 }),
			Act::CloseSettings => self.screen = Some(Screen::Root { sel: 0 }),
			Act::OpenWorlds => {
				self.pending_worlds.push(WorldIntent::Refresh);
				self.screen = Some(Screen::Worlds(WorldsState {
					rows: Vec::new(),
					sel: 0,
					status: None,
				}));
			}
			Act::WorldRow(idx) => {
				if let Some(Screen::Worlds(prev)) = self.screen.take() {
					if idx < prev.rows.len() {
						self.screen = Some(Screen::WorldActions { prev, idx, sel: 0 });
					} else {
						self.screen = Some(Screen::Worlds(prev));
					}
				}
			}
			Act::OpenCreate => {
				if let Some(Screen::Worlds(prev)) = self.screen.take() {
					self.screen = Some(Screen::CreateWorld {
						prev,
						name: String::new(),
					});
				}
			}
			Act::ConfirmCreate => {
				if let Some(Screen::CreateWorld { mut prev, name }) = self.screen.take() {
					let name = name.trim().to_string();
					if name.is_empty() {
						self.screen = Some(Screen::CreateWorld { prev, name });
						return;
					}
					self.pending_worlds.push(WorldIntent::Create { name });
					// The engine enters the new world synchronously (menu
					// closes via on_world_entered) or reports a status.
					prev.status = Some("creating world...".into());
					self.screen = Some(Screen::Worlds(prev));
				}
			}
			Act::EnterWorld => {
				if let Some(Screen::WorldActions { mut prev, idx, .. }) = self.screen.take() {
					if let Some(row) = prev.rows.get(idx) {
						self.pending_worlds
							.push(WorldIntent::Enter { id: row.id.clone() });
						prev.status = Some(format!("loading {}...", row.name));
					}
					self.screen = Some(Screen::Worlds(prev));
				}
			}
			Act::DeleteWorld => {
				if let Some(Screen::WorldActions { prev, idx, .. }) = self.screen.take() {
					self.screen = Some(Screen::ConfirmDelete { prev, idx, sel: 0 });
				}
			}
			Act::ConfirmDeleteWorld => {
				if let Some(Screen::ConfirmDelete { mut prev, idx, .. }) = self.screen.take() {
					if let Some(row) = prev.rows.get(idx) {
						self.pending_worlds
							.push(WorldIntent::Delete { id: row.id.clone() });
					}
					prev.sel = 0;
					self.screen = Some(Screen::Worlds(prev));
				}
			}
			Act::BackToWorlds => {
				if let Some(Screen::WorldActions { prev, .. })
				| Some(Screen::ConfirmDelete { prev, .. }) = self.screen.take()
				{
					self.screen = Some(Screen::Worlds(prev));
				}
			}
			Act::SettingRow(s) => {
				// Enter/click on the toggle row flips it; sliders react to
				// Left/Right and bar clicks instead.
				if s == Setting::Ssr {
					self.adjust_setting(s, 1);
				}
			}
			Act::JoinRow(i) => {
				let name = self.name.trim().to_string();
				if let Err(e) = digblock_net::protocol::validate_name(&name) {
					if let Some(Screen::Browser(b)) = &mut self.screen {
						b.status = Some(e.into());
					}
					return;
				}
				// Remember the last name actually joined with.
				if self.settings.name != name {
					self.settings.name = name.clone();
					self.settings_dirty = true;
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
				// With a world store, pre-entry play goes through the named
				// save list; the ephemeral "Play Local" survives only for
				// store-less runs (scripted flows, no-injection paths).
				if self.entered_world {
					acts.push((RowId::RootItem(0), Act::CloseMenu));
					if self.store_available && !self.connected {
						acts.push((RowId::RootItem(5), Act::OpenWorlds));
					}
				} else if self.store_available {
					acts.push((RowId::RootItem(5), Act::OpenWorlds));
				} else {
					acts.push((RowId::RootItem(0), Act::CloseMenu));
				}
				if self.services_available {
					acts.push((RowId::RootItem(1), Act::OpenMultiplayer));
				}
				if self.connected {
					acts.push((RowId::RootItem(2), Act::LeaveServer));
				}
				// Hidden until a world is entered; on cooldown the row
				// renders dim and is not an act.
				if self.entered_world && self.spawn_cooldown_secs == 0 {
					acts.push((RowId::RootItem(3), Act::ReturnSpawn));
				}
				acts.push((RowId::RootItem(4), Act::OpenSettings));
			}
			Some(Screen::Browser(b)) => {
				for i in 0..b.rows.len() {
					acts.push((RowId::Server(i), Act::JoinRow(i)));
				}
				acts.push((RowId::Refresh, Act::Refresh));
			}
			Some(Screen::Settings { .. }) => {
				for (i, s) in SETTINGS_ROWS.iter().enumerate() {
					acts.push((RowId::SettingsRow(i as u8), Act::SettingRow(*s)));
				}
				acts.push((RowId::SettingsRow(4), Act::CloseSettings));
			}
			Some(Screen::Worlds(w)) => {
				for i in 0..w.rows.len() {
					acts.push((RowId::World(i), Act::WorldRow(i)));
				}
				acts.push((RowId::CreateWorld, Act::OpenCreate));
			}
			Some(Screen::CreateWorld { .. }) => {
				acts.push((RowId::CreateWorld, Act::ConfirmCreate));
			}
			Some(Screen::WorldActions { prev, idx, .. }) => {
				let compatible = prev.rows.get(*idx).is_some_and(|r| r.compatible);
				if compatible {
					acts.push((RowId::WorldAction(0), Act::EnterWorld));
				}
				acts.push((RowId::WorldAction(1), Act::DeleteWorld));
				acts.push((RowId::WorldAction(2), Act::BackToWorlds));
			}
			Some(Screen::ConfirmDelete { .. }) => {
				acts.push((RowId::WorldAction(0), Act::ConfirmDeleteWorld));
				acts.push((RowId::WorldAction(1), Act::BackToWorlds));
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
		/// The panel never shrinks below this (short screens would jitter
		/// as rows change) and grows to fit its widest row's text.
		const MIN_PANEL_W: f32 = 560.0;
		/// Minimum gap between a row's text and its right-aligned column.
		const COL_GAP: f32 = 2.0 * crate::overlay::GLYPH_PX;
		let (w, h) = (size.0 as f32, size.1 as f32);
		let y0 = (h * 0.18).max(8.0);
		let mut y = y0;
		// Two passes: rows are collected with their y first, then placed
		// once the widest row has set the panel width.
		let mut specs: Vec<(RowSpec, f32)> = Vec::new();
		let push = |specs: &mut Vec<(RowSpec, f32)>, y: &mut f32, spec: RowSpec| {
			specs.push((spec, *y));
			*y += ROW_H;
		};

		match &self.screen {
			None => {}
			Some(Screen::Root { .. }) => {
				push(&mut specs, &mut y, RowSpec::title("DIGBLOCK"));
				y += ROW_H * 0.5;
				if self.entered_world {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(0), "Back to Game"),
					);
					if self.store_available && !self.connected {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(5), "Singleplayer"),
						);
					}
				} else if self.store_available {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(5), "Singleplayer"),
					);
				} else {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(0), "Play Local"),
					);
				}
				if self.services_available {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(1), "Multiplayer"),
					);
				} else {
					push(
						&mut specs,
						&mut y,
						RowSpec::dim("Multiplayer (no master server; run with --master-url)"),
					);
				}
				if self.connected {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(2), "Leave Server"),
					);
				}
				if self.entered_world {
					if self.spawn_cooldown_secs == 0 {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(3), "Return to Spawn"),
						);
					} else {
						push(
							&mut specs,
							&mut y,
							RowSpec::dim(format!(
								"Return to Spawn ({}s)",
								self.spawn_cooldown_secs
							)),
						);
					}
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::RootItem(4), "Settings"),
				);
				y += ROW_H * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::dim(if self.entered_world {
						"arrows + enter, esc: back to game"
					} else {
						"arrows + enter"
					}),
				);
			}
			Some(Screen::Settings { .. }) => {
				push(&mut specs, &mut y, RowSpec::title("SETTINGS"));
				y += ROW_H * 0.5;
				let st = &self.settings;
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(0),
						format!("Draw Distance: {}", st.draw_dist),
						(st.draw_dist - DRAW_MIN) as f32 / (DRAW_MAX - DRAW_MIN) as f32,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(1),
						format!("Music Volume: {:.0}%", st.music_vol * 100.0),
						st.music_vol / VOL_MAX,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(2),
						format!("SFX Volume: {:.0}%", st.sfx_vol * 100.0),
						st.sfx_vol / VOL_MAX,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(
						RowId::SettingsRow(3),
						if st.ssr {
							"Water Reflections: On"
						} else {
							"Water Reflections: Off"
						},
					),
				);
				y += ROW_H * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::SettingsRow(4), "Back"),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::dim("left/right or click adjusts, esc: back"),
				);
			}
			Some(Screen::Browser(b)) => {
				push(&mut specs, &mut y, RowSpec::title("MULTIPLAYER"));
				let caret = if blink { "_" } else { " " };
				push(
					&mut specs,
					&mut y,
					RowSpec::field(format!("name: {}{caret}", self.name)),
				);
				y += ROW_H * 0.25;
				if b.fetching && b.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("fetching server list..."));
				} else if b.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("no servers online"));
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
						&mut specs,
						&mut y,
						RowSpec {
							id: Some(RowId::Server(i)),
							text: format!(
								"{}  {}/{}",
								row.entry.name, row.entry.players, row.entry.max_players
							),
							right: Some((ping, color)),
							bar: None,
							color: C_WHITE,
						},
					);
				}
				push(&mut specs, &mut y, RowSpec::item(RowId::Refresh, "Refresh"));
				if let Some(status) = &b.status {
					push(&mut specs, &mut y, RowSpec::status(status.clone()));
				}
				y += ROW_H * 0.25;
				push(
					&mut specs,
					&mut y,
					RowSpec::dim("type your name, enter joins, esc: back"),
				);
			}
			Some(Screen::Connecting { label, .. }) => {
				push(&mut specs, &mut y, RowSpec::title("MULTIPLAYER"));
				y += ROW_H * 0.5;
				push(&mut specs, &mut y, RowSpec::dim(label.clone()));
				push(&mut specs, &mut y, RowSpec::dim("esc: cancel"));
			}
			Some(Screen::Worlds(w)) => {
				push(&mut specs, &mut y, RowSpec::title("SINGLEPLAYER"));
				y += ROW_H * 0.5;
				if w.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("no worlds yet"));
				}
				for (i, row) in w.rows.iter().enumerate() {
					push(
						&mut specs,
						&mut y,
						RowSpec {
							id: Some(RowId::World(i)),
							text: row.name.clone(),
							right: Some((
								row.detail.clone(),
								if row.compatible { C_DIM } else { C_RED },
							)),
							bar: None,
							color: if row.compatible { C_WHITE } else { C_DIM },
						},
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::CreateWorld, "Create New World"),
				);
				if let Some(status) = &w.status {
					push(&mut specs, &mut y, RowSpec::status(status.clone()));
				}
				y += ROW_H * 0.25;
				push(&mut specs, &mut y, RowSpec::dim("enter opens, esc: back"));
			}
			Some(Screen::CreateWorld { name, .. }) => {
				push(&mut specs, &mut y, RowSpec::title("NEW WORLD"));
				y += ROW_H * 0.5;
				let caret = if blink { "_" } else { " " };
				push(
					&mut specs,
					&mut y,
					RowSpec {
						id: Some(RowId::CreateWorld),
						text: format!("name: {name}{caret}"),
						right: None,
						bar: None,
						color: C_WHITE,
					},
				);
				y += ROW_H * 0.25;
				push(
					&mut specs,
					&mut y,
					RowSpec::dim("type a name, enter creates, esc: back"),
				);
			}
			Some(Screen::WorldActions { prev, idx, .. }) => {
				let (name, compatible) = prev
					.rows
					.get(*idx)
					.map_or(("?".to_string(), true), |r| (r.name.clone(), r.compatible));
				push(&mut specs, &mut y, RowSpec::title(&name));
				y += ROW_H * 0.5;
				if compatible {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::WorldAction(0), "Enter World"),
					);
				} else {
					push(
						&mut specs,
						&mut y,
						RowSpec::dim("from an old version - can't be entered"),
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(1), "Delete World"),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(2), "Back"),
				);
			}
			Some(Screen::ConfirmDelete { prev, idx, .. }) => {
				let name = prev.rows.get(*idx).map_or("?", |r| r.name.as_str());
				push(&mut specs, &mut y, RowSpec::title("DELETE WORLD"));
				y += ROW_H * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(0), &format!("Delete \"{name}\" forever")),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::WorldAction(1), "Cancel"),
				);
			}
		}

		// Width to content: the widest row (text plus any right-aligned
		// column or slider bar) fits with the overlay's own padding,
		// clamped to the window; MIN_PANEL_W keeps short screens from
		// looking cramped.
		let text_w = |s: &str| s.len() as f32 * crate::overlay::GLYPH_PX;
		let content_w = specs
			.iter()
			.map(|(spec, _)| {
				crate::overlay::TEXT_PAD_X * 2.0
					+ text_w(&spec.text)
					+ spec
						.right
						.as_ref()
						.map_or(0.0, |(right, _)| COL_GAP + text_w(right))
					+ spec.bar.map_or(0.0, |_| COL_GAP + BAR_W)
			})
			.fold(MIN_PANEL_W, f32::max);
		let pw = content_w.min(w - 16.0);
		let px = ((w - pw) / 2.0).max(8.0);
		let selected_id = self.acts().get(self.current_sel()).map(|a| a.0);
		let rows = specs
			.into_iter()
			.map(|(spec, ry)| PlacedRow {
				rect: [px, ry, pw, ROW_H],
				highlight: spec.id.is_some() && spec.id == selected_id,
				id: spec.id,
				text: spec.text,
				right: spec.right,
				bar: spec.bar,
				color: spec.color,
			})
			.collect();

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

/// Settings-screen row order (indices match `RowId::SettingsRow`).
const SETTINGS_ROWS: [Setting; 4] = [
	Setting::DrawDist,
	Setting::MusicVol,
	Setting::SfxVol,
	Setting::Ssr,
];

/// Slider-bar geometry, shared by hit-testing (`bar_frac_at`) and the
/// overlay's quads so they cannot drift.
pub(crate) const BAR_W: f32 = 200.0;
pub(crate) const BAR_H: f32 = 12.0;

/// The bar's rect within a placed slider row (right-aligned, centered
/// vertically).
pub(crate) fn bar_rect(row: [f32; 4]) -> [f32; 4] {
	let [x, y, w, h] = row;
	[
		x + w - crate::overlay::TEXT_PAD_X - BAR_W,
		y + (h - BAR_H) / 2.0,
		BAR_W,
		BAR_H,
	]
}

fn snap_vol(v: f32, dir: i32) -> f32 {
	let steps = (v / VOL_STEP).round() as i32 + dir;
	let max = (VOL_MAX / VOL_STEP).round() as i32;
	steps.clamp(0, max) as f32 * VOL_STEP
}

struct RowSpec {
	id: Option<RowId>,
	text: String,
	right: Option<(String, u32)>,
	/// Slider fill fraction 0..=1; Some = render a bar on this row.
	bar: Option<f32>,
	color: u32,
}

impl RowSpec {
	fn title(t: &str) -> Self {
		Self {
			id: None,
			text: t.into(),
			right: None,
			bar: None,
			color: C_WHITE,
		}
	}
	fn item(id: RowId, t: &str) -> Self {
		Self {
			id: Some(id),
			text: t.into(),
			right: None,
			bar: None,
			color: C_WHITE,
		}
	}
	fn slider(id: RowId, t: String, frac: f32) -> Self {
		Self {
			id: Some(id),
			text: t,
			right: None,
			bar: Some(frac.clamp(0.0, 1.0)),
			color: C_WHITE,
		}
	}
	fn field(t: String) -> Self {
		Self {
			id: None,
			text: t,
			right: None,
			bar: None,
			color: C_WHITE,
		}
	}
	fn status(t: String) -> Self {
		Self {
			id: None,
			text: t,
			right: None,
			bar: None,
			color: C_RED,
		}
	}
	fn dim(t: impl Into<String>) -> Self {
		Self {
			id: None,
			text: t.into(),
			right: None,
			bar: None,
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
	/// Slider fill fraction 0..=1 (settings screen); rendered as a
	/// right-aligned bar via [`bar_rect`].
	pub bar: Option<f32>,
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

	/// Default settings with a pre-filled name (most tests join as "ted").
	fn named(name: &str) -> Settings {
		Settings {
			name: name.into(),
			..Default::default()
		}
	}

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

	/// The panel sizes itself to its widest row: no screen may lay out
	/// text (plus any right-aligned column) past the row edge — the
	/// browser's hint line used to overflow the old fixed 560px width.
	#[test]
	fn panel_fits_every_row() {
		let check = |menu: &Menu| {
			let view = menu.view(SIZE, false);
			let [px, _, pw, _] = view.panel;
			for row in &view.rows {
				let [rx, _, rw, _] = row.rect;
				let mut need = crate::overlay::TEXT_PAD_X * 2.0
					+ row.text.len() as f32 * crate::overlay::GLYPH_PX;
				if let Some((right, _)) = &row.right {
					need += right.len() as f32 * crate::overlay::GLYPH_PX;
				}
				if row.bar.is_some() {
					need += BAR_W;
				}
				assert!(
					need <= rw,
					"{:?} overflows its row ({need} > {rw})",
					row.text
				);
				assert!(rx >= px && rx + rw <= px + pw, "row outside panel");
			}
		};

		// Root, with the long no-master hint.
		let menu = Menu::new(true, named("ted"), None);
		check(&menu);
		// Browser: name field, server rows with ping column, hint line.
		let mut menu = Menu::new(true, named("ted"), None);
		menu.set_services_available();
		open_browser(&mut menu);
		menu.on_response(MenuResponse::ServerList(Ok(vec![entry(
			"s1",
			"Alpha-Beta_0",
		)])));
		menu.on_response(MenuResponse::Ping {
			server_id: "s1".into(),
			ms: Some(1234),
		});
		check(&menu);
		// Settings: slider rows carry text plus a right-aligned bar.
		let mut menu = Menu::new(true, named("ted"), None);
		open_settings(&mut menu);
		check(&menu);
		// Worlds screens: list with detail column, create, actions,
		// confirm.
		let mut menu = Menu::new(true, named("ted"), None);
		open_worlds(
			&mut menu,
			vec![
				world_row("w1", "A-Long-World-Name", true),
				world_row("w2", "Old", false),
			],
		);
		check(&menu);
		menu.handle_key(Key::Enter); // actions
		check(&menu);
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter); // confirm delete
		check(&menu);
		menu.handle_key(Key::Escape); // back to list
		menu.set_sel(2); // create row
		menu.handle_key(Key::Enter);
		check(&menu);
	}

	#[test]
	fn disabled_menu_never_opens() {
		let mut menu = Menu::new(false, named("ted"), None);
		assert!(!menu.is_open());
		menu.open();
		assert!(!menu.is_open());
	}

	#[test]
	fn root_play_local_closes_and_escape_reopens() {
		let mut menu = Menu::new(true, named("ted"), None);
		assert!(menu.is_open());
		menu.handle_key(Key::Enter); // Play Local
		assert!(!menu.is_open());
		menu.open();
		assert!(menu.is_open());
	}

	#[test]
	fn multiplayer_requires_services() {
		let mut menu = Menu::new(true, named("ted"), None);
		// Startup, without services: Play Local + Settings only.
		assert_eq!(menu.acts().len(), 2);
		menu.set_services_available();
		assert_eq!(menu.acts().len(), 3);
	}

	#[test]
	fn startup_menu_locks_until_first_world() {
		let mut menu = Menu::new(true, named("ted"), None);
		// No Return to Spawn row (not even dim), and Escape is a no-op.
		let view = menu.view(SIZE, false);
		assert!(!view.rows.iter().any(|r| r.text.contains("Return to Spawn")));
		menu.handle_key(Key::Escape);
		assert!(menu.is_open(), "Escape must not dismiss the startup menu");

		// Entering the world (Play Local) unlocks both.
		menu.handle_key(Key::Enter);
		assert!(!menu.take_return_spawn(), "Enter was Play Local, not spawn");
		menu.open();
		let view = menu.view(SIZE, false);
		assert!(view.rows.iter().any(|r| r.text.contains("Return to Spawn")));
		assert!(view.rows.iter().any(|r| r.text.contains("Back to Game")));
		menu.handle_key(Key::Escape);
		assert!(!menu.is_open());
	}

	#[test]
	fn browser_flow_join_and_fail_returns_with_status() {
		let mut menu = Menu::new(true, named("ted"), None);
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
	fn invalid_name_blocks_join_with_alert() {
		for bad in ["", "ab"] {
			let mut menu = Menu::new(true, named(bad), None);
			menu.set_services_available();
			open_browser(&mut menu);
			let _ = menu.take_commands(); // drain the RefreshServers from opening
			menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
			menu.handle_key(Key::Enter);
			assert!(
				menu.take_commands().is_empty(),
				"join with {bad:?} not blocked"
			);
			let view = menu.view(SIZE, false);
			assert!(view.rows.iter().any(|r| r.text.contains("3-10 chars")));
		}
	}

	#[test]
	fn valid_name_joins() {
		let mut menu = Menu::new(true, Settings::default(), None);
		menu.set_services_available();
		open_browser(&mut menu);
		let _ = menu.take_commands();
		menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
		for ch in "Ted_99".chars() {
			menu.handle_text(ch);
		}
		menu.handle_key(Key::Enter);
		match menu.take_commands().as_slice() {
			[MenuCommand::Join { name, .. }] => assert_eq!(name, "Ted_99"),
			other => panic!("expected Join, got {} commands", other.len()),
		}
	}

	#[test]
	fn text_edits_name_with_charset_cap_and_backspace() {
		let mut menu = Menu::new(true, Settings::default(), None);
		menu.set_services_available();
		open_browser(&mut menu);
		// Punctuation, spaces, and non-ASCII never enter the buffer.
		for ch in "te d!é_x".chars() {
			menu.handle_text(ch);
		}
		menu.handle_key(Key::Backspace); // drop the trailing x
		assert!(menu
			.view(SIZE, false)
			.rows
			.iter()
			.any(|r| r.text.contains("name: ted_")));
		for _ in 0..100 {
			menu.handle_text('x');
		}
		let long = menu.view(SIZE, true);
		let name_row = long
			.rows
			.iter()
			.find(|r| r.text.starts_with("name:"))
			.unwrap();
		assert!(
			name_row.text.len() <= "name: ".len() + digblock_net::protocol::MAX_DISPLAY_NAME + 1
		);
	}

	#[test]
	fn welcome_closes_menu_and_cancel_restores_browser() {
		let mut menu = Menu::new(true, named("ted"), None);
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
		let mut menu = Menu::new(true, named("ted"), None);
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
		let mut menu = Menu::new(true, named("ted"), None);
		menu.set_connected(true);
		let acts = menu.acts();
		// Back to Game + Leave Server + Return to Spawn + Settings (no
		// services).
		assert_eq!(acts.len(), 4);
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		assert!(menu.take_disconnect());
		assert!(!menu.is_open());
	}

	#[test]
	fn return_to_spawn_ready_activates_and_cooldown_greys() {
		let mut menu = Menu::new(true, named("ted"), None);
		menu.handle_key(Key::Enter); // Play Local: the row now exists
		menu.open();
		// Ready: the row after Back to Game (no services); activating raises
		// the intent and closes the menu.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		assert!(menu.take_return_spawn());
		assert!(!menu.is_open());

		// On cooldown: dim countdown row, not selectable, no intent.
		menu.open();
		menu.set_spawn_cooldown(42);
		let acts = menu.acts();
		assert!(
			!acts.iter().any(|(id, _)| *id == RowId::RootItem(3)),
			"greyed spawn row is not an act"
		);
		assert_eq!(acts.len(), 2, "Back to Game + Settings");
		let view = menu.view(SIZE, false);
		assert!(view
			.rows
			.iter()
			.any(|r| r.text.contains("Return to Spawn (42s)")));
		// Down skips the greyed row entirely (lands on Settings): no
		// spawn intent is ever raised.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		assert!(!menu.take_return_spawn());
		menu.handle_key(Key::Escape); // out of Settings
		menu.handle_key(Key::Enter); // Back to Game
		assert!(!menu.take_return_spawn());
		assert!(!menu.is_open());

		// Cooldown over: selectable again.
		menu.open();
		menu.set_spawn_cooldown(0);
		assert_eq!(menu.acts().len(), 3);
	}

	fn open_settings(menu: &mut Menu) {
		// The Settings row is always the last act on Root.
		let last = menu.acts().len() - 1;
		menu.set_sel(last);
		menu.handle_key(Key::Enter);
	}

	#[test]
	fn settings_adjust_clamp_and_dirty_once() {
		let mut menu = Menu::new(true, Settings::default(), None);
		open_settings(&mut menu);
		assert!(
			menu.take_settings_change().is_none(),
			"opening isn't a change"
		);

		// Draw distance: sel 0, step 2, clamped at DRAW_MAX.
		menu.handle_key(Key::Right);
		let s = menu.take_settings_change().expect("dirty after adjust");
		assert_eq!(s.draw_dist, 22);
		assert!(menu.take_settings_change().is_none(), "drained once");
		for _ in 0..40 {
			menu.handle_key(Key::Right);
		}
		assert_eq!(menu.take_settings_change().unwrap().draw_dist, DRAW_MAX);
		// At the max, Right is a no-op — not a dirty change.
		menu.handle_key(Key::Right);
		assert!(menu.take_settings_change().is_none());

		// Music volume: sel 1, 100% -> 90%, floor at 0.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Left);
		let s = menu.take_settings_change().unwrap();
		assert!((s.music_vol - 0.9).abs() < 1e-4);
		for _ in 0..40 {
			menu.handle_key(Key::Left);
		}
		assert_eq!(menu.take_settings_change().unwrap().music_vol, 0.0);

		// SSR: sel 3, Enter toggles, Left also toggles.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		assert!(!menu.take_settings_change().unwrap().ssr);
		menu.handle_key(Key::Left);
		assert!(menu.take_settings_change().unwrap().ssr);

		// Escape returns to Root; the values held.
		menu.handle_key(Key::Escape);
		open_settings(&mut menu);
		let view = menu.view(SIZE, false);
		assert!(view
			.rows
			.iter()
			.any(|r| r.text.contains("Draw Distance: 32")));
		assert!(view
			.rows
			.iter()
			.any(|r| r.text.contains("Music Volume: 0%")));
	}

	#[test]
	fn settings_bar_click_sets_snapped_value() {
		let mut menu = Menu::new(true, Settings::default(), None);
		open_settings(&mut menu);
		let view = menu.view(SIZE, false);
		let row = view
			.rows
			.iter()
			.find(|r| r.id == Some(RowId::SettingsRow(0)))
			.expect("draw slider row");
		let [bx, by, bw, bh] = bar_rect(row.rect);

		// Click at 50% of the bar: snapped to the step grid around the
		// range midpoint (10..=32 -> 21 -> snaps to 20 or 22).
		let (cx, cy) = (bx + bw * 0.5, by + bh * 0.5);
		menu.handle_input(&InputEvent::CursorMoved { x: cx, y: cy }, SIZE);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
		);
		let s = menu
			.take_settings_change()
			.expect("bar click changes value");
		assert!(s.draw_dist == 20 || s.draw_dist == 22, "{}", s.draw_dist);
		assert_eq!((s.draw_dist - DRAW_MIN) % DRAW_STEP, 0);

		// Click at the far left end: the minimum.
		menu.handle_input(&InputEvent::CursorMoved { x: bx, y: cy }, SIZE);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
		);
		assert_eq!(menu.take_settings_change().unwrap().draw_dist, DRAW_MIN);

		// A click on the row but OUTSIDE the bar leaves the value alone
		// (sliders don't "activate").
		let (lx, ly) = (row.rect[0] + 5.0, row.rect[1] + 5.0);
		menu.handle_input(&InputEvent::CursorMoved { x: lx, y: ly }, SIZE);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
		);
		assert!(menu.take_settings_change().is_none());
	}

	#[test]
	fn join_remembers_name_in_settings() {
		let mut menu = Menu::new(true, Settings::default(), None);
		menu.set_services_available();
		open_browser(&mut menu);
		menu.take_commands();
		for ch in "Miner42".chars() {
			menu.handle_text(ch);
		}
		menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
		menu.handle_key(Key::Enter); // join row 0
		assert!(matches!(
			menu.take_commands().as_slice(),
			[MenuCommand::Join { name, .. }] if name == "Miner42"
		));
		let s = menu.take_settings_change().expect("join persists the name");
		assert_eq!(s.name, "Miner42");

		// Same name again next join: no redundant dirty.
		menu.on_net_failed("kicked");
		menu.handle_key(Key::Enter);
		menu.take_commands();
		assert!(menu.take_settings_change().is_none());
	}

	#[test]
	fn prefilled_name_from_settings() {
		let menu = Menu::new(true, named("Miner42"), None);
		assert_eq!(menu.name, "Miner42");
	}

	fn world_row(id: &str, name: &str, compatible: bool) -> WorldRow {
		WorldRow {
			id: id.into(),
			name: name.into(),
			detail: if compatible {
				"3d ago".into()
			} else {
				"old version".into()
			},
			compatible,
		}
	}

	fn open_worlds(menu: &mut Menu, rows: Vec<WorldRow>) {
		menu.set_store_available();
		menu.set_sel(0);
		menu.handle_key(Key::Enter); // Singleplayer (row 0 pre-entry)
		assert!(matches!(
			menu.take_world_intents().as_slice(),
			[WorldIntent::Refresh]
		));
		menu.set_world_list(rows);
	}

	#[test]
	fn store_replaces_play_local_until_entered() {
		let mut menu = Menu::new(true, Settings::default(), None);
		menu.set_store_available();
		let view = menu.view(SIZE, false);
		assert!(view.rows.iter().any(|r| r.text == "Singleplayer"));
		assert!(!view.rows.iter().any(|r| r.text == "Play Local"));
		// Enter opens the world list, not the game.
		menu.handle_key(Key::Enter);
		assert!(menu.is_open());
		assert!(matches!(
			menu.take_world_intents().as_slice(),
			[WorldIntent::Refresh]
		));
		// Entering a world unlocks Back to Game; Singleplayer stays for
		// switching.
		menu.on_world_entered();
		assert!(!menu.is_open());
		menu.open();
		let view = menu.view(SIZE, false);
		assert!(view.rows.iter().any(|r| r.text == "Back to Game"));
		assert!(view.rows.iter().any(|r| r.text == "Singleplayer"));
	}

	#[test]
	fn world_enter_flow_emits_intent() {
		let mut menu = Menu::new(true, Settings::default(), None);
		open_worlds(&mut menu, vec![world_row("w1", "Home", true)]);
		menu.handle_key(Key::Enter); // world row -> actions
		menu.handle_key(Key::Enter); // Enter World
		assert!(matches!(
			menu.take_world_intents().as_slice(),
			[WorldIntent::Enter { id }] if id == "w1"
		));
		// Engine opens it and reports success.
		menu.on_world_entered();
		assert!(!menu.is_open());
	}

	#[test]
	fn create_world_text_entry_and_intent() {
		let mut menu = Menu::new(true, Settings::default(), None);
		open_worlds(&mut menu, vec![]);
		menu.handle_key(Key::Enter); // "Create New World" (only act)
							   // Empty name: Enter is a no-op.
		menu.handle_key(Key::Enter);
		assert!(menu.take_world_intents().is_empty());
		// Charset + cap; backspace edits.
		for ch in "My World!".chars() {
			menu.handle_text(ch);
		}
		for ch in "abcdefghijklmnop".chars() {
			menu.handle_text(ch); // runs into the 16-char cap
		}
		menu.handle_key(Key::Backspace);
		let Some(Screen::CreateWorld { name, .. }) = &menu.screen else {
			panic!("on create screen");
		};
		// "MyWorld" (invalid chars untypeable) + fill to the 16 cap, minus
		// the backspace.
		assert_eq!(name, "MyWorldabcdefgh");
		menu.handle_key(Key::Enter);
		assert!(matches!(
			menu.take_world_intents().as_slice(),
			[WorldIntent::Create { name }] if name == "MyWorldabcdefgh"
		));
		assert!(
			matches!(&menu.screen, Some(Screen::Worlds(w)) if w.status.is_some()),
			"back on the list with a status while the engine works"
		);
	}

	#[test]
	fn delete_requires_confirm() {
		let mut menu = Menu::new(true, Settings::default(), None);
		open_worlds(&mut menu, vec![world_row("w1", "Home", true)]);
		menu.handle_key(Key::Enter); // actions
		menu.handle_key(Key::Down); // Delete World
		menu.handle_key(Key::Enter);
		assert!(menu.take_world_intents().is_empty(), "no delete yet");
		assert!(matches!(&menu.screen, Some(Screen::ConfirmDelete { .. })));
		// Cancel first.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		assert!(menu.take_world_intents().is_empty());
		assert!(matches!(&menu.screen, Some(Screen::Worlds(_))));
		// Now delete for real.
		menu.handle_key(Key::Enter); // actions
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter); // confirm screen
		menu.handle_key(Key::Enter); // "Delete forever" is row 0
		assert!(matches!(
			menu.take_world_intents().as_slice(),
			[WorldIntent::Delete { id }] if id == "w1"
		));
	}

	#[test]
	fn incompatible_world_offers_delete_only() {
		let mut menu = Menu::new(true, Settings::default(), None);
		open_worlds(&mut menu, vec![world_row("w1", "Old", false)]);
		menu.handle_key(Key::Enter); // actions
		let acts = menu.acts();
		assert_eq!(acts.len(), 2, "Delete + Back only");
		assert!(!acts.iter().any(|(id, _)| *id == RowId::WorldAction(0)));
		// Enter on row 0 must be Delete (→ confirm), never EnterWorld.
		menu.handle_key(Key::Enter);
		assert!(matches!(&menu.screen, Some(Screen::ConfirmDelete { .. })));
		assert!(menu.take_world_intents().is_empty());
	}

	#[test]
	fn left_server_relocks_startup_menu() {
		let mut menu = Menu::new(true, Settings::default(), None);
		menu.set_store_available();
		menu.set_connected(true);
		assert!(menu.entered_world);
		menu.on_left_server();
		assert!(menu.is_open());
		// Locked again: Escape can't dismiss, no Back to Game.
		menu.handle_key(Key::Escape);
		assert!(menu.is_open());
		let view = menu.view(SIZE, false);
		assert!(!view.rows.iter().any(|r| r.text == "Back to Game"));
		assert!(view.rows.iter().any(|r| r.text == "Singleplayer"));
	}
}
