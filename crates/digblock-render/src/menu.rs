//! Startup/pause menu: a pure state machine plus pixel layout — no I/O, no
//! GPU, fully unit-testable. The launcher owns HTTP and ping probing on a
//! service thread and talks over the `MenuServices` channels; the engine
//! routes input here while the menu is open and renders the `MenuView`
//! through the overlay. The world keeps running (and rendering) behind it.

use crate::settings::{
	Settings, DRAW_MAX, DRAW_MIN, DRAW_STEP, GI_SCALE_DIVISORS, UI_SCALE_MAX, UI_SCALE_MIN,
	UI_SCALE_STEP, VOL_MAX, VOL_STEP,
};
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
	GiScale,
	UiScale,
	MusicVol,
	SfxVol,
	Ssr,
}

#[derive(Clone, Copy)]
enum Act {
	CloseMenu,
	OpenMultiplayer,
	LeaveServer,
	/// The local-world counterpart of [`Act::LeaveServer`]: flush the
	/// save and go back to the locked title menu (engine drains
	/// [`Menu::take_leave_world`]).
	LeaveWorld,
	ReturnSpawn,
	/// Close the menu and open the inventory screen (engine drains
	/// [`Menu::take_inventory`]).
	OpenInventory,
	/// Close the menu and open the alchemy screen (engine drains
	/// [`Menu::take_alchemy`]).
	OpenAlchemy,
	/// Close the menu and open the character screen (engine drains
	/// [`Menu::take_character`]).
	OpenCharacter,
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
	/// `0..SETTINGS_ROWS.len()` are the settings in screen order;
	/// [`SETTINGS_BACK`] is the Back row.
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
	wants_leave_world: bool,
	wants_return_spawn: bool,
	wants_inventory: bool,
	wants_alchemy: bool,
	wants_character: bool,
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
			wants_leave_world: false,
			wants_return_spawn: false,
			wants_inventory: false,
			wants_alchemy: false,
			wants_character: false,
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

	/// The Leave World row was picked: the engine closes the local save
	/// and returns to the title.
	pub fn take_leave_world(&mut self) -> bool {
		std::mem::take(&mut self.wants_leave_world)
	}

	pub fn take_return_spawn(&mut self) -> bool {
		std::mem::take(&mut self.wants_return_spawn)
	}

	/// The Inventory row was picked: the engine opens the screen.
	pub fn take_inventory(&mut self) -> bool {
		std::mem::take(&mut self.wants_inventory)
	}

	/// The Alchemy row was picked: the engine opens the screen.
	pub fn take_alchemy(&mut self) -> bool {
		std::mem::take(&mut self.wants_alchemy)
	}

	/// The Character row was picked: the engine opens the screen.
	pub fn take_character(&mut self) -> bool {
		std::mem::take(&mut self.wants_character)
	}

	/// The full settings snapshot, once per change batch. The engine
	/// applies runtime effects (draw dist, SSR, volume) and persists.
	pub fn take_settings_change(&mut self) -> Option<Settings> {
		std::mem::take(&mut self.settings_dirty).then(|| self.settings.clone())
	}

	/// Adopt the density-derived lighting detail the engine picked at the
	/// first `set_scale_factor`, so the slider reads what is in use. Deliberately does NOT mark the settings dirty: nothing was
	/// chosen, so there is nothing to persist — the derived default should
	/// follow the display if the player later moves the window to another
	/// one, and a saved value would freeze it.
	pub fn set_gi_scale(&mut self, gi_scale_div: u32) {
		self.settings.gi_scale_div = gi_scale_div;
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

	/// Left a server or a local world for a fresh backdrop: back to the
	/// locked startup menu — there is no game to "go back" to until the
	/// player picks a save or a server again.
	pub fn on_returned_to_title(&mut self) {
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

	pub fn handle_input(&mut self, event: &InputEvent, size: (u32, u32), scale: f32) {
		match *event {
			InputEvent::Key { key, pressed } if pressed => self.handle_key(key),
			InputEvent::Text { ch } => self.handle_text(ch),
			InputEvent::CursorMoved { x, y } => {
				self.cursor = (x, y);
				// Hover moves the keyboard selection (single highlight).
				if let Some(i) = self.act_index_at(x, y, size, scale) {
					self.set_sel(i);
				}
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			} => {
				if let Some(i) = self.act_index_at(self.cursor.0, self.cursor.1, size, scale) {
					let act = self.acts()[i].1;
					// A click inside a slider's bar sets the value from the
					// click fraction instead of "activating" the row.
					if let Act::SettingRow(s) = act {
						if let Some(frac) =
							self.bar_frac_at(self.cursor.0, self.cursor.1, size, scale)
						{
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
	fn bar_frac_at(&self, x: f32, y: f32, size: (u32, u32), scale: f32) -> Option<f32> {
		let view = self.view(size, scale, false);
		let row = view.rows.iter().find(|r| {
			let [rx, ry, rw, rh] = r.rect;
			r.bar.is_some() && x >= rx && x < rx + rw && y >= ry && y < ry + rh
		})?;
		let [bx, _, bw, _] = bar_rect(row.rect, scale);
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
			// Steps through the DIVISOR list, not a continuous range —
			// see settings::GI_SCALE_DIVISORS. `dir` is +1 = sharper, so
			// it walks the list backwards (divisor 1 is full res).
			Setting::GiScale => {
				let i = GI_SCALE_DIVISORS
					.iter()
					.position(|&d| d == st.gi_scale_div)
					.unwrap_or(1) as i32;
				let n = GI_SCALE_DIVISORS.len() as i32;
				let v = GI_SCALE_DIVISORS[(i - dir).clamp(0, n - 1) as usize];
				std::mem::replace(&mut st.gi_scale_div, v) != v
			}
			Setting::UiScale => {
				let v = snap_range(st.ui_scale, dir, UI_SCALE_MIN, UI_SCALE_MAX, UI_SCALE_STEP);
				(std::mem::replace(&mut st.ui_scale, v) - v).abs() > f32::EPSILON
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
			Setting::GiScale => {
				let n = GI_SCALE_DIVISORS.len();
				// Left = coarsest, right = sharpest, matching every other
				// bar here (more fill = more of the thing).
				let i = ((1.0 - frac) * (n - 1) as f32).round() as usize;
				let v = GI_SCALE_DIVISORS[i.min(n - 1)];
				std::mem::replace(&mut st.gi_scale_div, v) != v
			}
			Setting::UiScale => {
				let span = UI_SCALE_MAX - UI_SCALE_MIN;
				let v = UI_SCALE_MIN + (frac * span / UI_SCALE_STEP).round() * UI_SCALE_STEP;
				let v = v.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
				(std::mem::replace(&mut st.ui_scale, v) - v).abs() > f32::EPSILON
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
		use digblock_net::protocol::{is_name_char, is_text_char, MAX_DISPLAY_NAME};
		match &mut self.screen {
			Some(Screen::Browser(b)) => {
				b.status = None;
				// Invalid characters (spaces, punctuation, non-ASCII) are
				// untypeable rather than rejected later. Display names are
				// deliberately narrower than chat — see is_name_char.
				if self.name.chars().count() < MAX_DISPLAY_NAME && is_name_char(ch) {
					self.name.push(ch);
				}
			}
			// World names never leave this machine, so they get the full
			// text alphabet; same untypeable-invalid-chars property, own cap.
			Some(Screen::CreateWorld { name, .. }) => {
				if name.chars().count() < WORLD_NAME_MAX && is_text_char(ch) {
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
			Act::LeaveWorld => {
				self.wants_leave_world = true;
				self.screen = None;
			}
			Act::ReturnSpawn => {
				self.wants_return_spawn = true;
				self.screen = None;
			}
			Act::OpenInventory => {
				self.wants_inventory = true;
				self.screen = None;
			}
			Act::OpenAlchemy => {
				self.wants_alchemy = true;
				self.screen = None;
			}
			Act::OpenCharacter => {
				self.wants_character = true;
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
				// In-game order: Back to Game, then the hotkey screens
				// (Return to Spawn, Inventory, Alchemy), then the one exit
				// row, with Settings last. Local and multiplayer read the
				// same in game — Singleplayer and Multiplayer belong to
				// the title menu, and reaching either from inside a world
				// now goes through Leave.
				if self.entered_world {
					acts.push((RowId::RootItem(0), Act::CloseMenu));
					// On cooldown the row renders dim and is not an act.
					if self.spawn_cooldown_secs == 0 {
						acts.push((RowId::RootItem(3), Act::ReturnSpawn));
					}
					acts.push((RowId::RootItem(6), Act::OpenInventory));
					acts.push((RowId::RootItem(7), Act::OpenAlchemy));
					acts.push((RowId::RootItem(9), Act::OpenCharacter));
					if self.connected {
						acts.push((RowId::RootItem(2), Act::LeaveServer));
					} else {
						acts.push((RowId::RootItem(8), Act::LeaveWorld));
					}
				} else {
					if self.store_available {
						acts.push((RowId::RootItem(5), Act::OpenWorlds));
					} else {
						acts.push((RowId::RootItem(0), Act::CloseMenu));
					}
					if self.services_available {
						acts.push((RowId::RootItem(1), Act::OpenMultiplayer));
					}
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
				acts.push((RowId::SettingsRow(SETTINGS_BACK), Act::CloseSettings));
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

	fn act_index_at(&self, x: f32, y: f32, size: (u32, u32), scale: f32) -> Option<usize> {
		let hit = self.view(size, scale, false).hits.hit(x, y)?;
		self.acts().iter().position(|(row_id, _)| *row_id == hit)
	}

	/// Build the frame's view. `blink` drives the name-field caret.
	pub fn view(&self, size: (u32, u32), scale: f32, blink: bool) -> MenuView {
		use crate::ui::theme;
		let row_h = theme::ROW_H * scale;
		// The panel never shrinks below this (short screens would jitter
		// as rows change) and grows to fit its widest row's text.
		let min_panel_w = theme::MIN_PANEL_W * scale;
		let glyph = crate::ui::scale::glyph_px(scale);
		let pad_x = crate::ui::scale::text_pad_x(scale);
		// Minimum gap between a row's text and its right-aligned column.
		let col_gap = 2.0 * glyph;
		let (w, h) = (size.0 as f32, size.1 as f32);
		let y0 = (h * 0.18).max(8.0);
		let mut y = y0;
		// Two passes: rows are collected with their y first, then placed
		// once the widest row has set the panel width.
		let mut specs: Vec<(RowSpec, f32)> = Vec::new();
		let push = |specs: &mut Vec<(RowSpec, f32)>, y: &mut f32, spec: RowSpec| {
			specs.push((spec, *y));
			*y += row_h;
		};

		match &self.screen {
			None => {}
			Some(Screen::Root { .. }) => {
				push(&mut specs, &mut y, RowSpec::title("DIGBLOCK"));
				y += row_h * 0.5;
				if self.entered_world {
					push(
						&mut specs,
						&mut y,
						RowSpec::item(RowId::RootItem(0), "Back to Game"),
					);
					if self.spawn_cooldown_secs == 0 {
						push(
							&mut specs,
							&mut y,
							RowSpec::item_hint(RowId::RootItem(3), "Return to Spawn", "H"),
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
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(6), "Inventory", "I"),
					);
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(7), "Alchemy", "K"),
					);
					push(
						&mut specs,
						&mut y,
						RowSpec::item_hint(RowId::RootItem(9), "Character", "C"),
					);
					if self.connected {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(2), "Leave Server"),
						);
					} else {
						push(
							&mut specs,
							&mut y,
							RowSpec::item(RowId::RootItem(8), "Leave World"),
						);
					}
				} else {
					if self.store_available {
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
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::RootItem(4), "Settings"),
				);
				y += row_h * 0.5;
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
				y += row_h * 0.5;
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
				// Row indices are POSITIONAL — they must match
				// SETTINGS_ROWS, which is what `acts()` walks. Inserting a
				// row means renumbering every one below it.
				{
					let n = GI_SCALE_DIVISORS.len();
					let i = GI_SCALE_DIVISORS
						.iter()
						.position(|&d| d == st.gi_scale_div)
						.unwrap_or(1);
					push(
						&mut specs,
						&mut y,
						RowSpec::slider(
							RowId::SettingsRow(1),
							format!("Lighting Detail: {}", gi_scale_label(st.gi_scale_div)),
							// Full bar = divisor 1 = sharpest, so more
							// fill means more of the thing, like every
							// other slider here.
							(n - 1 - i) as f32 / (n - 1) as f32,
						),
					);
				}
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(2),
						format!("Interface Scale: {:.0}%", st.ui_scale * 100.0),
						(st.ui_scale - UI_SCALE_MIN) / (UI_SCALE_MAX - UI_SCALE_MIN),
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(3),
						format!("Music Volume: {:.0}%", st.music_vol * 100.0),
						st.music_vol / VOL_MAX,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::slider(
						RowId::SettingsRow(4),
						format!("SFX Volume: {:.0}%", st.sfx_vol * 100.0),
						st.sfx_vol / VOL_MAX,
					),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::item(
						RowId::SettingsRow(5),
						if st.ssr {
							"Water Reflections: On"
						} else {
							"Water Reflections: Off"
						},
					),
				);
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::item(RowId::SettingsRow(SETTINGS_BACK), "Back"),
				);
				push(
					&mut specs,
					&mut y,
					RowSpec::dim("left/right or click adjusts, esc: back"),
				);
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec {
						// The build's version, right-aligned on the credits
						// row. `env!` here is digblock-render's version, which
						// inherits [workspace.package] like every other crate.
						right: Some((format!("v{}", env!("CARGO_PKG_VERSION")), C_DIM)),
						..RowSpec::dim("Created by @Zaneris (GitHub/X)")
					},
				);
			}
			Some(Screen::Browser(b)) => {
				push(&mut specs, &mut y, RowSpec::title("MULTIPLAYER"));
				push(
					&mut specs,
					&mut y,
					RowSpec::field(None, format!("name: {}", self.name), blink),
				);
				y += row_h * 0.25;
				if b.fetching && b.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("fetching server list..."));
				} else if b.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("no servers online"));
				}
				for (i, row) in b.rows.iter().enumerate() {
					let (ping, color) = match row.ping {
						PingState::Pending => ("...".to_string(), C_DIM),
						PingState::Failed => ("n/a".to_string(), C_RED),
						// Bands are round-trip times, and they line up with the
						// netbench's link profiles: green is lan/wifi
						// territory, indistinguishable from local; yellow
						// covers cellular, where delivery still holds up and
						// corrections stay small; red is bad-4g, where
						// delivery falls off even with the input hold. They
						// read as too lenient only if you remember them being
						// fed handshake times, which were roughly double an
						// RTT — the probe now reports the transport's own
						// estimate, so these mean what they say. Don't
						// "retune" them for the old input.
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
							right: Some((ping, color)),
							..RowSpec::new(
								Some(RowId::Server(i)),
								format!(
									"{}  {}/{}",
									row.entry.name, row.entry.players, row.entry.max_players
								),
								C_WHITE,
							)
						},
					);
				}
				push(&mut specs, &mut y, RowSpec::item(RowId::Refresh, "Refresh"));
				if let Some(status) = &b.status {
					push(&mut specs, &mut y, RowSpec::status(status.clone()));
				}
				y += row_h * 0.25;
				push(
					&mut specs,
					&mut y,
					RowSpec::dim("type your name, enter joins, esc: back"),
				);
			}
			Some(Screen::Connecting { label, .. }) => {
				push(&mut specs, &mut y, RowSpec::title("MULTIPLAYER"));
				y += row_h * 0.5;
				push(&mut specs, &mut y, RowSpec::dim(label.clone()));
				push(&mut specs, &mut y, RowSpec::dim("esc: cancel"));
			}
			Some(Screen::Worlds(w)) => {
				push(&mut specs, &mut y, RowSpec::title("SINGLEPLAYER"));
				y += row_h * 0.5;
				if w.rows.is_empty() {
					push(&mut specs, &mut y, RowSpec::dim("no worlds yet"));
				}
				for (i, row) in w.rows.iter().enumerate() {
					push(
						&mut specs,
						&mut y,
						RowSpec {
							right: Some((
								row.detail.clone(),
								if row.compatible { C_DIM } else { C_RED },
							)),
							..RowSpec::new(
								Some(RowId::World(i)),
								row.name.clone(),
								if row.compatible { C_WHITE } else { C_DIM },
							)
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
				y += row_h * 0.25;
				push(&mut specs, &mut y, RowSpec::dim("enter opens, esc: back"));
			}
			Some(Screen::CreateWorld { name, .. }) => {
				push(&mut specs, &mut y, RowSpec::title("NEW WORLD"));
				y += row_h * 0.5;
				push(
					&mut specs,
					&mut y,
					RowSpec::field(Some(RowId::CreateWorld), format!("name: {name}"), blink),
				);
				y += row_h * 0.25;
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
				y += row_h * 0.5;
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
				y += row_h * 0.5;
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
		// clamped to the window; min_panel_w keeps short screens from
		// looking cramped.
		// Must match what widgets::row actually draws — see
		// `panel_fits_every_row`, which re-derives this independently.
		let text_w = |s: &str| crate::font::ui().measure(s, glyph);
		let content_w = specs
			.iter()
			.map(|(spec, _)| {
				pad_x * 2.0
					+ text_w(&spec.text)
					// Reserved whether or not the caret is lit this frame,
					// so the panel doesn't breathe with the blink.
					+ if spec.caret {
						crate::ui::widgets::caret_w(glyph)
					} else {
						0.0
					} + spec
					.right
					.as_ref()
					.map_or(0.0, |(right, _)| col_gap + text_w(right))
					+ spec
						.bar
						.map_or(0.0, |_| col_gap + crate::ui::widgets::SLIDER_BAR_W * scale)
			})
			.fold(min_panel_w, f32::max);
		let pw = content_w.min(w - 16.0 * scale);
		let px = ((w - pw) / 2.0).max(8.0 * scale);
		let selected_id = self.acts().get(self.current_sel()).map(|a| a.0);
		let rows = specs
			.into_iter()
			.map(|(spec, ry)| PlacedRow {
				rect: [px, ry, pw, row_h],
				highlight: spec.id.is_some() && spec.id == selected_id,
				id: spec.id,
				text: spec.text,
				right: spec.right,
				bar: spec.bar,
				color: spec.color,
				caret: spec.caret,
			})
			.collect();

		let pad = theme::PAD * scale;
		let rows: Vec<PlacedRow> = rows;
		let mut hits = crate::ui::HitMap::default();
		for row in &rows {
			if let Some(id) = row.id {
				hits.push(row.rect.into(), id);
			}
		}
		MenuView {
			panel: [px - pad, y0 - pad, pw + pad * 2.0, (y - y0) + pad * 2.0],
			rows,
			hits,
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
/// Row index of the Settings screen's Back row: always one past the
/// adjustable rows. Derived, never a literal — adding a setting used to
/// mean remembering to bump it in two places or Back stopped clicking.
const SETTINGS_BACK: u8 = SETTINGS_ROWS.len() as u8;

// Render scale is deliberately ABSENT: it is locked at native for
// players and only `--render-scale` moves it (settings::RENDER_SCALE_MIN
// says why). Re-exposing it means a row here, a match arm in
// `adjust_setting` and `set_setting_frac`, a `RowSpec` below, and
// renumbering every row after it — the indices are positional.
const SETTINGS_ROWS: [Setting; 6] = [
	Setting::DrawDist,
	// Lighting detail sits next to draw distance: both trade picture for
	// frame time, and a player hunting fps should find them together
	// rather than one of them filed under "interface".
	Setting::GiScale,
	Setting::UiScale,
	Setting::MusicVol,
	Setting::SfxVol,
	Setting::Ssr,
];

/// The slider bar's rect within a placed row — the framework's
/// [`crate::ui::widgets::slider_rect`], in this module's `[f32; 4]`
/// vocabulary (shared by `bar_frac_at` and rendering, so no drift).
pub(crate) fn bar_rect(row: [f32; 4], scale: f32) -> [f32; 4] {
	crate::ui::widgets::slider_rect(row.into(), scale).arr()
}

/// Step a slider by `dir`, re-derived from its step grid rather than
/// added to the live value: repeated float addition drifts, and the
/// interface scale's finer step drifts faster than the volume's coarser
/// one.
fn snap_range(v: f32, dir: i32, min: f32, max: f32, step: f32) -> f32 {
	let steps = ((v - min) / step).round() as i32 + dir;
	let last = ((max - min) / step).round() as i32;
	min + steps.clamp(0, last) as f32 * step
}

fn snap_vol(v: f32, dir: i32) -> f32 {
	snap_range(v, dir, 0.0, VOL_MAX, VOL_STEP)
}

/// Player-facing name for a GI divisor. Named for the PICTURE rather than
/// the ratio ("Half" would read as half of the window, which it is not —
/// it is half of the render scale, itself already a fraction).
fn gi_scale_label(div: u32) -> &'static str {
	match div {
		1 => "Ultra",
		2 => "High",
		3 => "Medium",
		_ => "Low",
	}
}

struct RowSpec {
	id: Option<RowId>,
	text: String,
	right: Option<(String, u32)>,
	/// Slider fill fraction 0..=1; Some = render a bar on this row.
	bar: Option<f32>,
	color: u32,
	/// Text-entry caret at the end of `text`, drawn as a quad. NOT a
	/// character in the string: `_` and ` ` have different widths in a
	/// proportional font, so blinking one in and out would resize the
	/// panel (sized to its widest row) twice a second.
	caret: bool,
}

impl RowSpec {
	fn new(id: Option<RowId>, text: String, color: u32) -> Self {
		Self {
			id,
			text,
			right: None,
			bar: None,
			color,
			caret: false,
		}
	}
	fn title(t: &str) -> Self {
		Self::new(None, t.into(), C_WHITE)
	}
	fn item(id: RowId, t: &str) -> Self {
		Self::new(Some(id), t.into(), C_WHITE)
	}
	/// An item with a right-aligned dim keyboard hint ("I", "H", ...).
	fn item_hint(id: RowId, t: &str, hint: &str) -> Self {
		Self {
			right: Some((hint.into(), C_DIM)),
			..Self::new(Some(id), t.into(), C_WHITE)
		}
	}
	fn slider(id: RowId, t: String, frac: f32) -> Self {
		Self {
			bar: Some(frac.clamp(0.0, 1.0)),
			..Self::new(Some(id), t, C_WHITE)
		}
	}
	/// A text-entry row. `caret` is the blink phase.
	fn field(id: Option<RowId>, t: String, caret: bool) -> Self {
		Self {
			caret,
			..Self::new(id, t, C_WHITE)
		}
	}
	fn status(t: String) -> Self {
		Self::new(None, t, C_RED)
	}
	fn dim(t: impl Into<String>) -> Self {
		Self::new(None, t.into(), C_DIM)
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
	/// Text-entry caret at the end of `text` (blink phase already applied).
	pub caret: bool,
}

pub struct MenuView {
	/// Backdrop rect enclosing all rows.
	pub panel: [f32; 4],
	pub rows: Vec<PlacedRow>,
	/// Selectable rows' rects by id, filled by the same layout pass —
	/// hit-testing and rendering share one geometry source.
	pub hits: crate::ui::HitMap<RowId>,
}

impl MenuView {
	/// Emit the menu into the frame's draw list: full-screen dim (faded
	/// with the world's darkness — the panel keeps its fixed backdrop),
	/// panel backdrop, then rows through the framework's row widget.
	pub fn emit(
		&self,
		dl: &mut crate::ui::DrawList,
		style: &crate::ui::Style,
		size: (u32, u32),
		daylight: f32,
	) {
		use crate::ui::{theme, widgets, Layer, Rect};
		dl.quad(
			Layer::Content,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(theme::SCREEN_DIM, daylight),
		);
		widgets::panel(dl, Layer::Content, self.panel.into(), theme::PANEL_BG);
		for row in &self.rows {
			widgets::row(
				dl,
				style,
				Layer::Content,
				row.rect.into(),
				widgets::RowContent {
					text: &row.text,
					right: row.right.as_ref().map(|(s, c)| (s.as_str(), *c)),
					bar: row.bar,
					color: row.color,
					highlight: row.highlight,
					caret: row.caret,
				},
			);
		}
	}
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

	/// The in-game root offers Inventory (hint "I") and Return to Spawn
	/// (hint "H"); picking Inventory raises the intent and closes.
	#[test]
	fn inventory_row_raises_intent_and_hotkey_hints_show() {
		let mut menu = Menu::new(true, named("ted"), None);
		assert!(
			!menu
				.view(SIZE, 1.0, false)
				.rows
				.iter()
				.any(|r| r.text == "Inventory"),
			"no inventory row before entering a world"
		);
		menu.handle_key(Key::Enter); // Play Local
		menu.open();
		let view = menu.view(SIZE, 1.0, false);
		let hint = |name: &str| {
			view.rows
				.iter()
				.find(|r| r.text == name)
				.and_then(|r| r.right.as_ref())
				.map(|(s, _)| s.as_str())
		};
		assert_eq!(hint("Inventory"), Some("I"));
		assert_eq!(hint("Alchemy"), Some("K"));
		assert_eq!(hint("Return to Spawn"), Some("H"));

		menu.handle_key(Key::Down); // Return to Spawn
		menu.handle_key(Key::Down); // Inventory
		menu.handle_key(Key::Enter);
		assert!(menu.take_inventory());
		assert!(!menu.take_inventory(), "intent is one-shot");
		assert!(!menu.is_open(), "menu closed for the screen");
	}

	/// The Alchemy row mirrors Inventory: raises its intent and closes.
	#[test]
	fn alchemy_row_raises_intent() {
		let mut menu = Menu::new(true, named("ted"), None);
		menu.handle_key(Key::Enter); // Play Local
		menu.open();
		menu.handle_key(Key::Down); // Return to Spawn
		menu.handle_key(Key::Down); // Inventory
		menu.handle_key(Key::Down); // Alchemy
		menu.handle_key(Key::Enter);
		assert!(menu.take_alchemy());
		assert!(!menu.take_alchemy(), "intent is one-shot");
		assert!(!menu.is_open(), "menu closed for the screen");
	}

	/// Connected to a server, the Multiplayer row disappears entirely —
	/// joining another server goes through Leave Server first.
	#[test]
	fn multiplayer_hidden_while_connected() {
		let mut menu = Menu::new(true, named("ted"), None);
		menu.set_services_available();
		menu.set_connected(true);
		assert!(!menu
			.acts()
			.iter()
			.any(|(_, act)| matches!(act, Act::OpenMultiplayer)));
		assert!(!menu
			.view(SIZE, 1.0, false)
			.rows
			.iter()
			.any(|r| r.text.starts_with("Multiplayer")));
		// Leaving brings it back (fresh backdrop world, startup menu).
		menu.on_returned_to_title();
		assert!(menu
			.acts()
			.iter()
			.any(|(_, act)| matches!(act, Act::OpenMultiplayer)));
	}

	/// The view emits through the framework: dim + panel + one text
	/// command per row (all Content layer), and the hit map built by the
	/// same pass resolves a row's center to its id.
	#[test]
	fn view_emits_draw_commands_and_hit_map_agrees() {
		use crate::ui::draw::{Cmd, Layer};
		let menu = Menu::new(true, named("ted"), None);
		let view = menu.view(SIZE, 1.0, false);
		let mut dl = crate::ui::DrawList::default();
		view.emit(&mut dl, &crate::ui::Style::new(1.0), SIZE, 1.0);
		let (cmds, arena) = dl.cmds(Layer::Content);
		let texts: Vec<&str> = cmds
			.iter()
			.filter_map(|c| match c {
				Cmd::Text { range, .. } => Some(&arena[range.0 as usize..range.1 as usize]),
				_ => None,
			})
			.collect();
		assert_eq!(texts.len(), view.rows.len(), "one text per row");
		assert!(texts.contains(&"DIGBLOCK"));
		// Dim + panel quads lead the layer.
		assert!(matches!(cmds[0], Cmd::Quad { .. }) && matches!(cmds[1], Cmd::Quad { .. }));

		// Hit map: a selectable row's center resolves to its id.
		let row = view.rows.iter().find(|r| r.id.is_some()).unwrap();
		let [x, y, w, h] = row.rect;
		assert_eq!(view.hits.hit(x + w / 2.0, y + h / 2.0), row.id);
		assert_eq!(view.hits.hit(1.0, 1.0), None, "outside the panel");
	}

	/// The panel sizes itself to its widest row: no screen may lay out
	/// text (plus any right-aligned column) past the row edge — the
	/// browser's hint line used to overflow the old fixed panel width.
	#[test]
	fn panel_fits_every_row() {
		let check = |menu: &Menu| {
			let view = menu.view(SIZE, 1.0, false);
			let [px, _, pw, _] = view.panel;
			for row in &view.rows {
				let [rx, _, rw, _] = row.rect;
				let glyph = crate::ui::scale::glyph_px(1.0);
				let w = |s: &str| crate::font::ui().measure(s, glyph);
				let mut need = crate::ui::scale::text_pad_x(1.0) * 2.0 + w(&row.text);
				if row.caret {
					need += crate::ui::widgets::caret_w(glyph);
				}
				if let Some((right, _)) = &row.right {
					need += w(right);
				}
				if row.bar.is_some() {
					need += crate::ui::widgets::SLIDER_BAR_W;
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
		let view = menu.view(SIZE, 1.0, false);
		assert!(!view.rows.iter().any(|r| r.text.contains("Return to Spawn")));
		menu.handle_key(Key::Escape);
		assert!(menu.is_open(), "Escape must not dismiss the startup menu");

		// Entering the world (Play Local) unlocks both.
		menu.handle_key(Key::Enter);
		assert!(!menu.take_return_spawn(), "Enter was Play Local, not spawn");
		menu.open();
		let view = menu.view(SIZE, 1.0, false);
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
		let view = menu.view(SIZE, 1.0, false);
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
		let view = menu.view(SIZE, 1.0, false);
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
			let view = menu.view(SIZE, 1.0, false);
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
			.view(SIZE, 1.0, false)
			.rows
			.iter()
			.any(|r| r.text.contains("name: ted_")));
		for _ in 0..100 {
			menu.handle_text('x');
		}
		let long = menu.view(SIZE, 1.0, true);
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

		// Second run: cancel instead. From a fresh (pre-entry) menu —
		// Multiplayer lives on the title menu, not the in-game one.
		let mut menu = Menu::new(true, named("ted"), None);
		menu.set_services_available();
		menu.handle_key(Key::Down); // Multiplayer (after Play Local)
		menu.handle_key(Key::Enter);
		menu.on_response(MenuResponse::ServerList(Ok(vec![entry("s1", "Alpha")])));
		menu.handle_key(Key::Enter);
		let _ = menu.take_commands();
		menu.handle_key(Key::Escape); // cancel connect
		assert!(menu.take_disconnect());
		assert!(menu
			.view(SIZE, 1.0, false)
			.rows
			.iter()
			.any(|r| r.text.contains("cancelled")));
	}

	#[test]
	fn click_hits_rows() {
		let mut menu = Menu::new(true, named("ted"), None);
		menu.set_services_available();
		// Click "Multiplayer" (second selectable row).
		let view = menu.view(SIZE, 1.0, false);
		let mp_row = view
			.rows
			.iter()
			.find(|r| r.id == Some(RowId::RootItem(1)))
			.unwrap();
		let (cx, cy) = (mp_row.rect[0] + 5.0, mp_row.rect[1] + 5.0);
		menu.handle_input(&InputEvent::CursorMoved { x: cx, y: cy }, SIZE, 1.0);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
			1.0,
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
		// Back to Game + Return to Spawn + Inventory + Alchemy +
		// Character + Leave Server + Settings (no services).
		assert_eq!(acts.len(), 7);
		menu.handle_key(Key::Down); // Return to Spawn
		menu.handle_key(Key::Down); // Inventory
		menu.handle_key(Key::Down); // Alchemy
		menu.handle_key(Key::Down); // Character
		menu.handle_key(Key::Down); // Leave Server
		menu.handle_key(Key::Enter);
		assert!(menu.take_disconnect());
		assert!(!menu.is_open());
	}

	/// The two in-game menus used to diverge: a local world offered
	/// Singleplayer and Multiplayer where a server offered Leave Server,
	/// so the same Escape key produced a different page depending on
	/// where you were. Both now end in exactly one exit row.
	#[test]
	fn leave_world_is_the_local_counterpart_of_leave_server() {
		let rows_of = |menu: &Menu| -> Vec<String> {
			menu.view(SIZE, 1.0, false)
				.rows
				.iter()
				.map(|r| r.text.to_string())
				.collect()
		};
		let mut local = Menu::new(true, named("ted"), None);
		local.set_services_available();
		local.set_store_available();
		local.on_world_entered();
		local.open();
		let mut server = Menu::new(true, named("ted"), None);
		server.set_services_available();
		server.set_store_available();
		server.set_connected(true);

		let (a, b) = (rows_of(&local), rows_of(&server));
		assert_eq!(
			a.iter().position(|r| r == "Leave World"),
			b.iter().position(|r| r == "Leave Server"),
			"the exit row sits in the same place: {a:?} vs {b:?}"
		);
		for rows in [&a, &b] {
			assert!(!rows.iter().any(|r| r == "Singleplayer"), "{rows:?}");
			assert!(
				!rows.iter().any(|r| r.starts_with("Multiplayer")),
				"{rows:?}"
			);
		}
		assert_eq!(a.len(), b.len(), "same page either side: {a:?} vs {b:?}");

		// Activating it raises the intent and closes the menu, exactly
		// as Leave Server does.
		let leave = local
			.acts()
			.iter()
			.position(|(id, _)| *id == RowId::RootItem(8))
			.expect("the Leave World row is an act");
		local.set_sel(leave);
		local.handle_key(Key::Enter);
		assert!(local.take_leave_world());
		assert!(!local.take_disconnect(), "a local world has no transport");
		assert!(!local.is_open());
	}

	#[test]
	fn return_to_spawn_ready_activates_and_cooldown_greys() {
		let mut menu = Menu::new(true, named("ted"), None);
		menu.handle_key(Key::Enter); // Play Local: the row now exists
		menu.open();
		// Ready: directly after Back to Game (no services); activating
		// raises the intent and closes the menu.
		menu.handle_key(Key::Down); // Return to Spawn
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
		assert_eq!(
			acts.len(),
			6,
			"Back to Game + Inventory + Alchemy + Character + Leave World + Settings"
		);
		let view = menu.view(SIZE, 1.0, false);
		assert!(view
			.rows
			.iter()
			.any(|r| r.text.contains("Return to Spawn (42s)")));
		// Down skips the greyed row entirely: no spawn intent is ever
		// raised on the way to Settings.
		menu.handle_key(Key::Down); // Inventory (greyed spawn skipped)
		menu.handle_key(Key::Down); // Alchemy
		menu.handle_key(Key::Down); // Character
		menu.handle_key(Key::Down); // Leave World
		menu.handle_key(Key::Down); // Settings
		menu.handle_key(Key::Enter);
		assert!(!menu.take_return_spawn());
		menu.handle_key(Key::Escape); // out of Settings
		menu.handle_key(Key::Enter); // Back to Game
		assert!(!menu.take_return_spawn());
		assert!(!menu.is_open());

		// Cooldown over: selectable again.
		menu.open();
		menu.set_spawn_cooldown(0);
		assert_eq!(menu.acts().len(), 7, "spawn row is back");
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

		// Draw distance: sel 0, one step per press, clamped at DRAW_MAX.
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

		// Lighting detail: sel 1. Render scale has no row — it is locked
		// at native for players (settings::RENDER_SCALE_MIN says why), so
		// nothing here can move it. Walks the DIVISOR list, and Left is
		// coarser — so it ends on the largest divisor, not the smallest.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Left);
		assert_eq!(menu.take_settings_change().unwrap().gi_scale_div, 3);
		for _ in 0..10 {
			menu.handle_key(Key::Left);
		}
		assert_eq!(
			menu.take_settings_change().unwrap().gi_scale_div,
			*GI_SCALE_DIVISORS.last().unwrap()
		);
		menu.handle_key(Key::Left);
		assert!(menu.take_settings_change().is_none(), "already coarsest");
		// And back up to full res.
		for _ in 0..10 {
			menu.handle_key(Key::Right);
		}
		assert_eq!(menu.take_settings_change().unwrap().gi_scale_div, 1);

		// Interface scale: sel 2, one notch down from neutral, then on
		// down to the min.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Left);
		let s = menu.take_settings_change().unwrap();
		assert!(
			(s.ui_scale - (1.0 - UI_SCALE_STEP)).abs() < 1e-4,
			"{}",
			s.ui_scale
		);
		for _ in 0..40 {
			menu.handle_key(Key::Left);
		}
		assert!((menu.take_settings_change().unwrap().ui_scale - UI_SCALE_MIN).abs() < 1e-4);
		menu.handle_key(Key::Left);
		assert!(menu.take_settings_change().is_none(), "already at the min");

		// Music volume: sel 3, one notch down from full, floor at silence.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Left);
		let s = menu.take_settings_change().unwrap();
		assert!((s.music_vol - 0.9).abs() < 1e-4);
		for _ in 0..40 {
			menu.handle_key(Key::Left);
		}
		assert_eq!(menu.take_settings_change().unwrap().music_vol, 0.0);

		// SSR: sel 5, Enter toggles, Left also toggles.
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Down);
		menu.handle_key(Key::Enter);
		assert!(!menu.take_settings_change().unwrap().ssr);
		menu.handle_key(Key::Left);
		assert!(menu.take_settings_change().unwrap().ssr);

		// Escape returns to Root; the values held.
		menu.handle_key(Key::Escape);
		open_settings(&mut menu);
		let view = menu.view(SIZE, 1.0, false);
		assert!(view
			.rows
			.iter()
			.any(|r| r.text.contains("Draw Distance: 32")));
		assert!(view
			.rows
			.iter()
			.any(|r| r.text.contains("Music Volume: 0%")));
		assert!(view.rows.iter().any(|r| r
			.text
			.contains(&format!("Interface Scale: {:.0}%", UI_SCALE_MIN * 100.0))));
		// No render-scale row exists at all.
		assert!(!view.rows.iter().any(|r| r.text.contains("Render Scale")));
		assert!(view
			.rows
			.iter()
			.any(|r| r.text.contains("Lighting Detail: Ultra")));
	}

	/// Every notch of the interface-scale slider is reachable and lands on
	/// its exact grid value. With a fine step the old
	/// add-to-the-live-value path drifts, and the label rounds to whole
	/// percent — so a run to one end and back is the test that would
	/// catch it.
	#[test]
	fn ui_scale_slider_walks_its_whole_range_without_drift() {
		let notches = ((UI_SCALE_MAX - UI_SCALE_MIN) / UI_SCALE_STEP).round() as i32;
		let mut v = 1.0f32;
		let mut seen = Vec::new();
		for _ in 0..notches * 2 {
			v = snap_range(v, -1, UI_SCALE_MIN, UI_SCALE_MAX, UI_SCALE_STEP);
		}
		assert_eq!(v, UI_SCALE_MIN, "bottoms out exactly");
		for _ in 0..notches {
			seen.push(v);
			v = snap_range(v, 1, UI_SCALE_MIN, UI_SCALE_MAX, UI_SCALE_STEP);
		}
		assert_eq!(v, UI_SCALE_MAX, "tops out exactly");
		seen.push(v);
		assert_eq!(seen.len() as i32, notches + 1);
		for (i, got) in seen.iter().enumerate() {
			let want = UI_SCALE_MIN + i as f32 * UI_SCALE_STEP;
			assert!((got - want).abs() < 1e-6, "notch {i}: {got} vs {want}");
			// The label rounds to whole percent; no two notches may print
			// the same number.
			assert_eq!(
				format!("{:.0}", got * 100.0).parse::<i32>().unwrap(),
				70 + 5 * i as i32
			);
		}
	}

	#[test]
	fn settings_bar_click_sets_snapped_value() {
		let mut menu = Menu::new(true, Settings::default(), None);
		open_settings(&mut menu);
		let view = menu.view(SIZE, 1.0, false);
		let row = view
			.rows
			.iter()
			.find(|r| r.id == Some(RowId::SettingsRow(0)))
			.expect("draw slider row");
		let [bx, by, bw, bh] = bar_rect(row.rect, 1.0);

		// Click at the middle of the bar: snapped to the step grid around
		// the range midpoint, so it lands on a notch either side of it.
		let (cx, cy) = (bx + bw * 0.5, by + bh * 0.5);
		menu.handle_input(&InputEvent::CursorMoved { x: cx, y: cy }, SIZE, 1.0);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
			1.0,
		);
		let s = menu
			.take_settings_change()
			.expect("bar click changes value");
		assert!(s.draw_dist == 20 || s.draw_dist == 22, "{}", s.draw_dist);
		assert_eq!((s.draw_dist - DRAW_MIN) % DRAW_STEP, 0);

		// Click at the far left end: the minimum.
		menu.handle_input(&InputEvent::CursorMoved { x: bx, y: cy }, SIZE, 1.0);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
			1.0,
		);
		assert_eq!(menu.take_settings_change().unwrap().draw_dist, DRAW_MIN);

		// A click on the row but OUTSIDE the bar leaves the value alone
		// (sliders don't "activate").
		let (lx, ly) = (row.rect[0] + 5.0, row.rect[1] + 5.0);
		menu.handle_input(&InputEvent::CursorMoved { x: lx, y: ly }, SIZE, 1.0);
		menu.handle_input(
			&InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			},
			SIZE,
			1.0,
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
		let view = menu.view(SIZE, 1.0, false);
		assert!(view.rows.iter().any(|r| r.text == "Singleplayer"));
		assert!(!view.rows.iter().any(|r| r.text == "Play Local"));
		// Enter opens the world list, not the game.
		menu.handle_key(Key::Enter);
		assert!(menu.is_open());
		assert!(matches!(
			menu.take_world_intents().as_slice(),
			[WorldIntent::Refresh]
		));
		// Entering a world unlocks Back to Game and swaps Singleplayer
		// for Leave World: switching saves goes back through the title.
		menu.on_world_entered();
		assert!(!menu.is_open());
		menu.open();
		let view = menu.view(SIZE, 1.0, false);
		assert!(view.rows.iter().any(|r| r.text == "Back to Game"));
		assert!(view.rows.iter().any(|r| r.text == "Leave World"));
		assert!(!view.rows.iter().any(|r| r.text == "Singleplayer"));
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
			menu.handle_text(ch); // runs into the name-length cap
		}
		menu.handle_key(Key::Backspace);
		let Some(Screen::CreateWorld { name, .. }) = &menu.screen else {
			panic!("on create screen");
		};
		// World names take the full text alphabet (space and punctuation
		// included — they never leave this machine), filled to the cap,
		// minus the backspace.
		assert_eq!(name, "My World!abcdef");
		menu.handle_key(Key::Enter);
		assert!(matches!(
			menu.take_world_intents().as_slice(),
			[WorldIntent::Create { name }] if name == "My World!abcdef"
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
		menu.on_returned_to_title();
		assert!(menu.is_open());
		// Locked again: Escape can't dismiss, no Back to Game.
		menu.handle_key(Key::Escape);
		assert!(menu.is_open());
		let view = menu.view(SIZE, 1.0, false);
		assert!(!view.rows.iter().any(|r| r.text == "Back to Game"));
		assert!(view.rows.iter().any(|r| r.text == "Singleplayer"));
	}
}
