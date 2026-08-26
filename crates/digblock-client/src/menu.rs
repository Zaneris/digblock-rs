//! Startup/pause menu: a pure state machine plus pixel layout — no I/O, no
//! GPU, fully unit-testable. The launcher owns HTTP and ping probing on a
//! service thread and talks over the `MenuServices` channels; the engine
//! routes input here while the menu is open and renders the `MenuView`
//! through the overlay. The world keeps running (and rendering) behind it.

mod view;

use crate::settings::{
	DRAW_MAX, DRAW_MIN, DRAW_STEP, GI_SCALE_DIVISORS, LIGHT_CAP_MAX, LIGHT_CAP_MIN, LIGHT_CAP_STEP,
	Settings, UI_SCALE_MAX, UI_SCALE_MIN, UI_SCALE_STEP, VOL_MAX, VOL_STEP,
};
use digblock_net::probe::Ping;
use digblock_net::protocol::Compat;
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
	/// What the master holds for this server, read through
	/// [`digblock_net::protocol::compat`] before a join is offered — the
	/// list must not promise what the handshake would refuse.
	pub protocol_version: u16,
	pub sim_version: u16,
	/// Whether play here counts toward the hiscores board — the master's
	/// trusted list, published so the row can say so before a join.
	pub reports_skills: bool,
}

impl ServerEntry {
	/// Asked by `acts`, by the row that draws it, and by each launcher
	/// before it spends a probe — so nothing offers what the handshake
	/// would refuse.
	pub fn compat(&self) -> Compat {
		digblock_net::protocol::compat(self.protocol_version, self.sim_version)
	}
}

/// Who the player is signed in as, as far as anything drawable is concerned.
///
/// Deliberately not the session token: that lives in the launcher's services thread, the only place
/// that talks to the master, so this module stays a state machine with no I/O and no secrets. A
/// leak of everything the menu holds leaks a public name and a public id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AccountView {
	pub id: String,
	/// `None` until the player picks one, which is exactly when the menu
	/// must send them to the name screen instead of the server list.
	pub name: Option<String>,
}

/// Where the signed-in account stands on one of the master's boards (`GET /api/hiscores/me`):
/// `skill` is `None` for the overall board. A board the account is not on sends no row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RankView {
	pub skill: Option<digblock_core::skills::Skill>,
	pub rank: u32,
	pub level: u32,
	pub xp: u64,
}

/// One page of one of the master's boards (`GET /api/hiscores`): `skill` is `None` for the
/// overall board, where a row's level and XP are sums. `total` is how many accounts the board
/// holds, so the screen can page; `page_size` is what the master actually cut, which is what the
/// pager divides by — never the size that was asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BoardView {
	pub skill: Option<digblock_core::skills::Skill>,
	pub page: u32,
	pub page_size: u32,
	pub total: u32,
	pub rows: Vec<BoardRow>,
}

/// One ranked account. `me` is the launcher's verdict — it holds the session and so the account
/// id, which the engine deliberately never sees.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BoardRow {
	pub rank: u32,
	pub name: String,
	pub level: u32,
	pub xp: u64,
	pub me: bool,
}

/// Rows one page of the hiscores screen holds. Asked of the master as `size=`, which clamps it
/// to its own ceiling — fewer than the web page's rows, because the screen sits in a panel above
/// the hotbar with no scrolling, and a page that outgrows the window would need the fit ladder.
pub const BOARD_PAGE_ROWS: u32 = 10;

/// Pages [`Menu::test_open`] can jump to.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TestPage {
	SignIn,
	ChooseName,
	Account,
}

/// Menu → launcher requests, serviced on the launcher's thread.
#[derive(Debug)]
pub enum MenuCommand {
	RefreshServers,
	/// Mint a one-time token for `server_id`, connect, and answer `MenuResponse::Connect`.
	Join {
		server_id: String,
	},
	/// Open the browser at the master's sign-in page and wait — long-running, so never block on it.
	SignIn,
	/// Abandon a sign-in in progress.
	CancelSignIn,
	/// Live availability check while the player types a name.
	CheckName {
		name: String,
	},
	/// Claim a name for the signed-in account.
	SetName {
		name: String,
	},
	SignOut,
	/// Where the signed-in account ranks, for the character screen; answered with `Ranks`.
	FetchRanks,
	/// One page of one board for the hiscores screen; answered with `Board`. Public on the master,
	/// so it needs no session.
	FetchBoard {
		skill: Option<digblock_core::skills::Skill>,
		page: u32,
	},
}

/// Launcher → menu results.
pub enum MenuResponse {
	ServerList(Result<Vec<ServerEntry>, String>),
	/// Client-side reachability probe result; None = failed/timed out.
	Ping {
		server_id: String,
		ping: Option<Ping>,
	},
	/// Ready-to-use transport plus the minted token for Hello.
	Connect {
		transport: Box<dyn NetTransport>,
		token: [u8; 16],
	},
	JoinFailed {
		reason: String,
	},
	/// A sign-in finished, a session was restored, or a sign-out happened; `None` means signed out.
	Account(Option<AccountView>),
	SignInFailed {
		reason: String,
	},
	/// Answer to `CheckName`. Carries its own name: a stale answer must not label the current text.
	NameAvailable {
		name: String,
		available: bool,
		reason: Option<String>,
	},
	/// Answer to `SetName`: the claimed name, or why not.
	NameSet(Result<String, String>),
	/// Answer to `FetchRanks`. The ENGINE consumes this before the menu
	/// sees it: the board is the character screen's, not a menu page's.
	Ranks(Result<Vec<RankView>, String>),
	/// Answer to `FetchBoard`, consumed by the engine the same way: the hiscores screen's, not a
	/// menu page's. Carries the board and page it answers, so a stale answer can be told apart.
	Board(Result<BoardView, String>),
}

/// The channel pair the launcher hands the engine. Keeps HTTP entirely out
/// of the engine (wasm-clean) and off the main thread.
pub struct MenuServices {
	pub cmd_tx: std::sync::mpsc::Sender<MenuCommand>,
	pub resp_rx: std::sync::mpsc::Receiver<MenuResponse>,
}

/// Menu → engine intents for named local worlds (take-pattern, not
/// `MenuServices` — the world store is engine-local and synchronous).
#[derive(Debug)]
pub(crate) enum WorldIntent {
	Refresh,
	Create { name: String },
	Enter { id: String },
	Delete { id: String },
}

/// One saved world as the menu shows it. The engine builds these from
/// the store's `WorldMeta` (formatting the last-played label — the menu
/// stays clock-free).
#[derive(Clone)]
pub(crate) struct WorldRow {
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
	Ms(Ping),
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

struct SignInState {
	/// Set once the launcher reports a failure; until then the screen just
	/// says it is waiting.
	status: Option<String>,
	/// Whether to go on to Multiplayer once signed in. False when the
	/// player opened sign-in from the Account screen (a re-sign-in), where
	/// landing back on Account is what they expect.
	then_multiplayer: bool,
}

struct NameState {
	name: String,
	/// The last availability answer, and which name it was about. Compared
	/// against `name` before it is shown, so a reply that arrives after two
	/// more keystrokes does not label the current text.
	checked: Option<(String, bool, Option<String>)>,
	/// The name a `CheckName` is currently out for, if any. At most one is
	/// ever in flight — see `check_typed_name`.
	checking: Option<String>,
	status: Option<String>,
	/// True while a `SetName` is in flight, so the row reads as busy and a
	/// second Enter does not queue a duplicate claim.
	claiming: bool,
	/// A rename (from the Account screen) rather than a first pick, which
	/// only changes the wording and where Escape goes.
	rename: bool,
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
	/// Waiting on the browser. Reached from Multiplayer while signed out.
	SignIn(SignInState),
	/// First sign-in, or a rename: claim a display name — account-level, not re-typed per join.
	ChooseName(NameState),
	/// Signed-in account: the id to hand an operator, and what you can do to the account in-game.
	Account {
		sel: usize,
	},
}

/// One adjustable row on the Settings screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Setting {
	DrawDist,
	GiScale,
	MaxLights,
	UiScale,
	MusicVol,
	SfxVol,
	Ssr,
	/// Touch jump verb. Touch-HUD-only row: `acts()` and `view()` skip it in lockstep off-touch.
	TouchJump,
}

#[derive(Clone, Copy)]
enum Act {
	CloseMenu,
	/// Toggle browser fullscreen — the shell must call inside the activating gesture's dispatch.
	ToggleFullscreen,
	OpenMultiplayer,
	LeaveServer,
	/// The local-world counterpart of [`Act::LeaveServer`]: flush the save, back to the title menu.
	LeaveWorld,
	/// Close the menu and open the spellbook screen (engine drains [`Menu::take_spellbook`]).
	OpenSpellbook,
	/// Close the menu and open the inventory screen (engine drains [`Menu::take_inventory`]).
	OpenInventory,
	/// Close the menu and open the alchemy screen (engine drains [`Menu::take_alchemy`]).
	OpenAlchemy,
	/// Close the menu and open the character screen (engine drains [`Menu::take_character`]).
	OpenCharacter,
	/// Close the menu and open the hiscores screen (engine drains [`Menu::take_hiscores`]).
	OpenHiscores,
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
	/// Start the browser sign-in; `then_multiplayer` says whether Multiplayer was the way in.
	StartSignIn {
		then_multiplayer: bool,
	},
	CancelSignIn,
	/// Claim the typed name.
	ConfirmName,
	OpenAccount,
	/// Put the account id on the clipboard (engine drains [`Menu::take_clipboard`] from `input`).
	CopyAccountId,
	OpenRename,
	SignOut,
	/// Escape from the account screen back to the root menu.
	CloseAccount,
}

/// Stable identity linking selectable actions to laid-out rows, so
/// hit-testing and rendering can't drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowId {
	RootItem(u8),
	Server(usize),
	Refresh,
	/// `0..SETTINGS_ROWS.len()` are the settings in screen order; [`SETTINGS_BACK`] is Back.
	SettingsRow(u8),
	/// One saved world in the Singleplayer list.
	World(usize),
	/// The "Create New World" row (list) / the name field (create).
	CreateWorld,
	/// Rows on the actions/confirm screens, top to bottom.
	WorldAction(u8),
	/// The sign-in screen's single action (Cancel).
	SignInCancel,
	/// The name field on the name screen.
	NameField,
	/// Rows on the Account screen, top to bottom.
	AccountRow(u8),
}

pub(crate) struct Menu {
	/// Config gate: false (bench/screenshot/drill/iOS) = never opens.
	enabled: bool,
	/// A master URL is configured and the launcher wired services.
	services_available: bool,
	/// A world store was injected: Root offers "Singleplayer" (named
	/// saves) instead of the ephemeral "Play Local".
	store_available: bool,
	/// This build draws the touch HUD, so the touch-only settings rows
	/// are surfaced. See [`Menu::set_touch`].
	touch: bool,
	/// The draw-distance slider's lower bound on THIS platform
	/// (`EngineConfig::draw_min` — see [`Menu::set_draw_min`]). A bound on
	/// the slider only; `Settings::clamped` accepts down to the global
	/// floor everywhere.
	draw_min: i32,
	/// The shell can toggle browser fullscreen: Root offers the row.
	fullscreen_available: bool,
	/// There is no Fullscreen API, but an installed home-screen app would
	/// have no browser chrome: Root says so where the row would be.
	fullscreen_hint: bool,
	/// The Fullscreen row was picked; drained by the engine from `input`
	/// (the shell's call needs the activating gesture — see
	/// [`Act::ToggleFullscreen`]).
	wants_fullscreen: bool,
	pending_worlds: Vec<WorldIntent>,
	screen: Option<Screen>,
	/// Who we are signed in as; `None` = signed out. Set by the engine
	/// from the launcher's answers — the menu never derives it.
	account: Option<AccountView>,
	/// Engine-maintained: currently connected to a server?
	connected: bool,
	/// The player has entered the world at least once (picked Play Local
	/// or completed a join). Until then there is no game to go back to:
	/// the in-game rows are hidden and Escape can't dismiss the menu.
	entered_world: bool,
	pending_cmds: Vec<MenuCommand>,
	/// Whether the server the player last chose to join counts toward the
	/// hiscores — the MASTER's answer off the listing, remembered so the
	/// engine can say so at the door. Never the server's own claim: a
	/// modified server would claim whatever it liked.
	join_ranked: Option<bool>,
	wants_disconnect: bool,
	wants_leave_world: bool,
	wants_spellbook: bool,
	wants_inventory: bool,
	wants_alchemy: bool,
	wants_character: bool,
	wants_hiscores: bool,
	/// Text the player asked to copy, drained by the engine. A `String`
	/// rather than a call because this module does no I/O — see
	/// [`Menu::take_clipboard`] for why the engine drains it from `input`
	/// rather than `update`.
	wants_copy: Option<String>,
	/// Live settings, edited on the Settings screen.
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
	pub(crate) fn new(
		enabled: bool,
		settings: Settings,
		auto_join: Option<String>,
		account: Option<AccountView>,
	) -> Self {
		Self {
			enabled,
			services_available: false,
			store_available: false,
			touch: false,
			draw_min: DRAW_MIN,
			fullscreen_available: false,
			fullscreen_hint: false,
			wants_fullscreen: false,
			pending_worlds: Vec::new(),
			screen: enabled.then_some(Screen::Root { sel: 0 }),
			account,
			connected: false,
			entered_world: false,
			pending_cmds: Vec::new(),
			join_ranked: None,
			wants_disconnect: false,
			wants_leave_world: false,
			wants_spellbook: false,
			wants_inventory: false,
			wants_alchemy: false,
			wants_character: false,
			wants_hiscores: false,
			wants_copy: None,
			settings,
			settings_dirty: false,
			cursor: (-1.0, -1.0),
			auto_join,
		}
	}

	pub(crate) fn set_services_available(&mut self) {
		self.services_available = true;
		if self.enabled && self.auto_join.is_some() {
			self.activate(Act::OpenMultiplayer);
		}
	}

	/// This build draws the touch HUD: surface the touch-only settings
	/// rows. Same pattern as `set_store_available` — a capability the
	/// engine declares after construction, not a constructor parameter.
	pub(crate) fn set_touch(&mut self) {
		self.touch = true;
	}

	/// This platform's draw-distance slider floor (`EngineConfig::
	/// draw_min`). Same declared-capability pattern as [`Menu::set_touch`]
	/// — here it opens (or keeps closed) the slider's lower rungs rather
	/// than surfacing a row.
	pub(crate) fn set_draw_min(&mut self, min: i32) {
		self.draw_min = min;
	}

	/// The shell can toggle browser fullscreen: Root offers the row.
	pub(crate) fn set_fullscreen_available(&mut self) {
		self.fullscreen_available = true;
	}

	/// No Fullscreen API, but installing the page would drop the browser's
	/// chrome: Root says how, in place of the row it cannot offer.
	pub(crate) fn set_fullscreen_hint(&mut self) {
		self.fullscreen_hint = true;
	}

	/// The Fullscreen row was picked. Drained from `Engine::input` like
	/// the clipboard — the shell's fullscreen call is only honoured inside
	/// the same task as the activating gesture.
	pub(crate) fn take_fullscreen(&mut self) -> bool {
		std::mem::take(&mut self.wants_fullscreen)
	}

	/// A menu text field is accepting produced text (the display-name and
	/// world-name screens — exactly the ones `handle_text` feeds). Drives
	/// the shells' soft-keyboard bridges through
	/// `Game::text_entry_active`.
	pub(crate) fn text_field_open(&self) -> bool {
		matches!(
			self.screen,
			Some(Screen::ChooseName(_)) | Some(Screen::CreateWorld { .. })
		)
	}

	/// The open field's contents, seeding the soft-keyboard bridge.
	pub(crate) fn text_field_contents(&self) -> &str {
		match &self.screen {
			Some(Screen::ChooseName(n)) => &n.name,
			Some(Screen::CreateWorld { name, .. }) => name,
			_ => "",
		}
	}

	/// Whole-field replacement from the soft-keyboard bridge: clear, then
	/// feed every character through `handle_text`, so charset and cap
	/// cannot fork from the keystroke path. The trailing availability
	/// check covers pure deletion, which feeds no characters.
	pub(crate) fn handle_text_replace(&mut self, text: &str) {
		match &mut self.screen {
			Some(Screen::ChooseName(n)) => {
				n.status = None;
				n.name.clear();
			}
			Some(Screen::CreateWorld { name, .. }) => name.clear(),
			_ => return,
		}
		for ch in text.chars() {
			self.handle_text(ch);
		}
		self.check_typed_name();
	}

	pub(crate) fn is_open(&self) -> bool {
		self.screen.is_some()
	}

	/// Would Escape do anything here? The touch HUD asks before drawing its back button, so the one
	/// place that can't go back — the locked startup menu, with no world behind it — offers none.
	///
	/// Derived from the same match `handle_key` runs rather than a list of screens: a page that
	/// gains a way out gains the button with it.
	pub(crate) fn can_go_back(&self) -> bool {
		match &self.screen {
			None => false,
			Some(Screen::Root { .. }) => self.entered_world,
			Some(_) => true,
		}
	}

	/// In-game Escape: reopen at the root (no-op unless enabled).
	pub(crate) fn open(&mut self) {
		if self.enabled && self.screen.is_none() {
			self.screen = Some(Screen::Root { sel: 0 });
		}
	}

	pub(crate) fn set_connected(&mut self, connected: bool) {
		self.connected = connected;
		// Being on a server implies the startup menu is behind us.
		self.entered_world |= connected;
	}

	pub(crate) fn take_commands(&mut self) -> Vec<MenuCommand> {
		std::mem::take(&mut self.pending_cmds)
	}

	pub(crate) fn take_disconnect(&mut self) -> bool {
		std::mem::take(&mut self.wants_disconnect)
	}

	/// The Leave World row was picked: the engine closes the local save
	/// and returns to the title.
	pub(crate) fn take_leave_world(&mut self) -> bool {
		std::mem::take(&mut self.wants_leave_world)
	}

	/// The Spellbook row was picked: the engine opens the screen.
	pub(crate) fn take_spellbook(&mut self) -> bool {
		std::mem::take(&mut self.wants_spellbook)
	}

	/// The Inventory row was picked: the engine opens the screen.
	pub(crate) fn take_inventory(&mut self) -> bool {
		std::mem::take(&mut self.wants_inventory)
	}

	/// The Alchemy row was picked: the engine opens the screen.
	pub(crate) fn take_alchemy(&mut self) -> bool {
		std::mem::take(&mut self.wants_alchemy)
	}

	/// The Character row was picked: the engine opens the screen.
	pub(crate) fn take_character(&mut self) -> bool {
		std::mem::take(&mut self.wants_character)
	}

	/// The Hiscores row was picked: the engine opens the screen.
	pub(crate) fn take_hiscores(&mut self) -> bool {
		std::mem::take(&mut self.wants_hiscores)
	}

	/// The full settings snapshot, once per change batch. The engine
	/// applies runtime effects (draw dist, SSR, volume) and persists.
	pub(crate) fn take_settings_change(&mut self) -> Option<Settings> {
		std::mem::take(&mut self.settings_dirty).then(|| self.settings.clone())
	}

	/// Text the player asked to put on the clipboard.
	///
	/// The engine drains this from `Engine::input`, immediately after routing the event here — NOT
	/// from `update`. On web the clipboard write must happen in the same task as the click that
	/// asked for it, or the browser refuses it as unprompted; `update` runs a frame later.
	pub(crate) fn take_clipboard(&mut self) -> Option<String> {
		self.wants_copy.take()
	}

	pub(crate) fn account(&self) -> Option<&AccountView> {
		self.account.as_ref()
	}

	/// Open one of the account-flow pages directly, for the cross-screen theme conformance tests in
	/// `ui::screens`. These three pages are otherwise unreachable from a test — each needs a
	/// signed-in account and a live services thread to arrive at — and a screen the theme tests
	/// cannot reach is exactly the kind that has regressed unnoticed (see
	/// `every_screen_draws_text_at_one_size`).
	#[cfg(test)]
	pub(crate) fn test_open(&mut self, page: TestPage) {
		self.account = Some(AccountView {
			id: "b2379408-3102-8b68-b56b-5774dec269b8".into(),
			name: Some("Miner42".into()),
		});
		self.screen = Some(match page {
			TestPage::SignIn => Screen::SignIn(SignInState {
				status: Some("sign-in timed out".into()),
				then_multiplayer: true,
			}),
			TestPage::ChooseName => Screen::ChooseName(NameState {
				name: "Miner42".into(),
				checked: Some(("Miner42".into(), false, Some("that name is taken".into()))),
				checking: None,
				status: None,
				claiming: false,
				rename: true,
			}),
			TestPage::Account => Screen::Account { sel: 0 },
		});
	}

	/// Adopt the density-derived lighting detail the engine picked at the
	/// first `set_scale_factor`, so the slider reads what is in use. Deliberately does NOT mark the settings dirty: nothing was
	/// chosen, so there is nothing to persist — the derived default should
	/// follow the display if the player later moves the window to another
	/// one, and a saved value would freeze it.
	pub(crate) fn set_gi_scale(&mut self, gi_scale_div: u32) {
		self.settings.gi_scale_div = gi_scale_div;
	}

	/// World-store intents raised since the last drain.
	pub(crate) fn take_world_intents(&mut self) -> Vec<WorldIntent> {
		std::mem::take(&mut self.pending_worlds)
	}

	/// A world store was injected: Root offers named saves.
	pub(crate) fn set_store_available(&mut self) {
		self.store_available = true;
	}

	/// The store's world list (engine answers a `WorldIntent::Refresh`,
	/// and re-sends after create/delete).
	pub(crate) fn set_world_list(&mut self, rows: Vec<WorldRow>) {
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
	pub(crate) fn set_worlds_status(&mut self, status: String) {
		match &mut self.screen {
			Some(Screen::Worlds(w)) => w.status = Some(status),
			Some(Screen::CreateWorld { prev, .. })
			| Some(Screen::WorldActions { prev, .. })
			| Some(Screen::ConfirmDelete { prev, .. }) => prev.status = Some(status),
			_ => {}
		}
	}

	/// A local save opened: the menu's job here is done.
	pub(crate) fn on_world_entered(&mut self) {
		self.entered_world = true;
		self.screen = None;
	}

	/// Left a server or a local world for a fresh backdrop: back to the
	/// locked startup menu — there is no game to "go back" to until the
	/// player picks a save or a server again.
	pub(crate) fn on_returned_to_title(&mut self) {
		self.connected = false;
		self.entered_world = false;
		if self.enabled {
			self.screen = Some(Screen::Root { sel: 0 });
		}
	}

	// ---- engine hooks -----------------------------------------------------

	/// Welcome landed: the join finished, close the menu.
	pub(crate) fn on_welcome(&mut self) {
		if matches!(self.screen, Some(Screen::Connecting { .. })) {
			self.entered_world = true;
			self.screen = None;
		}
	}

	/// Refused/Disconnected while a join was in flight: back to the browser
	/// with the reason on the status line.
	pub(crate) fn on_net_failed(&mut self, reason: &str) {
		if matches!(self.screen, Some(Screen::Connecting { .. })) {
			let Some(Screen::Connecting { mut prev, .. }) = self.screen.take() else {
				return;
			};
			prev.status = Some(reason.to_string());
			self.screen = Some(Screen::Browser(prev));
		}
	}

	/// Whether the server the player last chose to join is on the master's
	/// trusted list; `None` before any join was chosen.
	pub(crate) fn join_ranked(&self) -> Option<bool> {
		self.join_ranked
	}

	/// Feed one service response in; a `Connect` hands the transport+token
	/// back for the engine to install.
	pub(crate) fn on_response(
		&mut self,
		resp: MenuResponse,
	) -> Option<(Box<dyn NetTransport>, [u8; 16])> {
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
			MenuResponse::Ping { server_id, ping } => {
				if let Some(Screen::Browser(b)) = &mut self.screen
					&& let Some(row) = b.rows.iter_mut().find(|r| r.entry.id == server_id)
				{
					row.ping = match ping {
						Some(ping) => PingState::Ms(ping),
						None => PingState::Failed,
					};
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

			// A sign-in landed, a stored session was restored at startup,
			// or we signed out. Only ADVANCE the screen when the player is
			// standing on the one that was waiting for this — a session
			// restored while they are reading Settings must not yank them
			// somewhere else.
			MenuResponse::Account(account) => {
				self.account = account;
				if let Some(Screen::SignIn(s)) = &self.screen {
					let then_multiplayer = s.then_multiplayer;
					match &self.account {
						None => {
							self.screen = Some(Screen::Root { sel: 0 });
						}
						Some(a) if a.name.is_none() => self.open_name_screen(false),
						Some(_) if then_multiplayer => self.open_browser(),
						Some(_) => self.screen = Some(Screen::Account { sel: 0 }),
					}
				}
			}
			MenuResponse::SignInFailed { reason } => {
				if let Some(Screen::SignIn(s)) = &mut self.screen {
					s.status = Some(reason);
				}
			}
			MenuResponse::NameAvailable {
				name,
				available,
				reason,
			} => self.name_answered(name, available, reason),
			MenuResponse::Ranks(_) | MenuResponse::Board(_) => {}
			MenuResponse::NameSet(result) => {
				let Some(Screen::ChooseName(n)) = &mut self.screen else {
					return None;
				};
				n.claiming = false;
				match result {
					Ok(name) => {
						let rename = n.rename;
						if let Some(a) = &mut self.account {
							a.name = Some(name);
						}
						// A first pick was on the way to Multiplayer; a
						// rename was a visit to the Account screen.
						if rename {
							self.screen = Some(Screen::Account { sel: 0 });
						} else {
							self.open_browser();
						}
					}
					Err(e) => n.status = Some(e),
				}
			}
		}
		None
	}

	// ---- input ------------------------------------------------------------

	pub(crate) fn handle_input(&mut self, event: &InputEvent, size: (u32, u32), scale: f32) {
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
					if let Act::SettingRow(s) = act
						&& let Some(frac) =
							self.bar_frac_at(self.cursor.0, self.cursor.1, size, scale)
					{
						self.set_setting_frac(s, frac);
						return;
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
			Some(Screen::Account { sel }) => *sel,
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
			Some(Screen::Account { sel: s }) => *s = sel,
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
				Some(Screen::ChooseName(n)) => {
					n.status = None;
					n.name.pop();
					self.check_typed_name();
				}
				Some(Screen::CreateWorld { name, .. }) => {
					name.pop();
				}
				_ => {}
			},
			Key::Left | Key::Right => {
				// Slider adjust on the Settings screen only.
				if matches!(self.screen, Some(Screen::Settings { .. }))
					&& let Some(&(_, Act::SettingRow(s))) = self.acts().get(self.current_sel())
				{
					self.adjust_setting(s, if key == Key::Left { -1 } else { 1 });
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
				Some(Screen::SignIn(_)) => {
					self.screen = Some(Screen::Root { sel: 0 });
					self.pending_cmds.push(MenuCommand::CancelSignIn);
				}
				// Backing out of a rename returns to Account. Backing out
				// of a FIRST pick returns to the root: there is no account
				// screen worth showing for an account that cannot join
				// anything yet, and the player can start over from
				// Multiplayer.
				Some(Screen::ChooseName(n)) => {
					self.screen = Some(if n.rename {
						Screen::Account { sel: 0 }
					} else {
						Screen::Root { sel: 0 }
					});
				}
				Some(Screen::Account { .. }) => self.screen = Some(Screen::Root { sel: 0 }),
				None => {}
			},
			_ => {}
		}
	}

	/// Step one setting by `dir` (±1). Toggles just flip; sliders move by
	/// their step, snapped to the step grid so float drift can't build up.
	fn adjust_setting(&mut self, s: Setting, dir: i32) {
		let draw_min = self.draw_min;
		let st = &mut self.settings;
		let changed = match s {
			Setting::DrawDist => {
				let v = (st.draw_dist + dir * DRAW_STEP).clamp(draw_min, DRAW_MAX);
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
			Setting::MaxLights => {
				let v = (st.max_lights + dir * LIGHT_CAP_STEP).clamp(LIGHT_CAP_MIN, LIGHT_CAP_MAX);
				std::mem::replace(&mut st.max_lights, v) != v
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
			Setting::TouchJump => {
				st.touch_jump_button = !st.touch_jump_button;
				true
			}
		};
		self.settings_dirty |= changed;
	}

	/// Set one slider from a 0..=1 bar fraction (mouse click), snapped.
	fn set_setting_frac(&mut self, s: Setting, frac: f32) {
		let frac = frac.clamp(0.0, 1.0);
		let draw_min = self.draw_min;
		let st = &mut self.settings;
		let changed = match s {
			Setting::DrawDist => {
				let span = (DRAW_MAX - draw_min) as f32;
				let v = draw_min
					+ ((frac * span / DRAW_STEP as f32).round() as i32 * DRAW_STEP)
						.clamp(0, DRAW_MAX - draw_min);
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
			Setting::MaxLights => {
				let span = (LIGHT_CAP_MAX - LIGHT_CAP_MIN) as f32;
				let v = LIGHT_CAP_MIN
					+ ((frac * span / LIGHT_CAP_STEP as f32).round() as i32 * LIGHT_CAP_STEP)
						.clamp(0, LIGHT_CAP_MAX - LIGHT_CAP_MIN);
				std::mem::replace(&mut st.max_lights, v) != v
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
			// Not sliders; a bar click can't reach them.
			Setting::Ssr | Setting::TouchJump => false,
		};
		self.settings_dirty |= changed;
	}

	fn handle_text(&mut self, ch: char) {
		use digblock_net::protocol::{MAX_DISPLAY_NAME, is_name_char, is_text_char};
		match &mut self.screen {
			// Invalid characters (spaces, punctuation, non-ASCII) are
			// untypeable rather than rejected later. Display names are
			// deliberately narrower than chat — see is_name_char.
			Some(Screen::ChooseName(n)) => {
				n.status = None;
				if n.name.chars().count() < MAX_DISPLAY_NAME && is_name_char(ch) {
					n.name.push(ch);
					self.check_typed_name();
				}
			}
			// World names never leave this machine, so they get the full
			// text alphabet; same untypeable-invalid-chars property, own cap.
			Some(Screen::CreateWorld { name, .. })
				if name.chars().count() < WORLD_NAME_MAX && is_text_char(ch) =>
			{
				name.push(ch);
			}
			_ => {}
		}
	}

	/// Ask the master whether the typed name is free. Considered per keystroke, but AT MOST ONE is
	/// ever out at a time: a keystroke landing mid-flight starts nothing, and the reply re-fires
	/// for whatever the text says by then (`name_answered`) — a debounce without a clock, which
	/// the menu does not have. It has to be here rather than in a filler: both fillers serve
	/// commands in order, so N queued checks put N HTTP calls ahead of the Join pressed next.
	fn check_typed_name(&mut self) {
		let Some(Screen::ChooseName(n)) = &mut self.screen else {
			return;
		};
		let name = n.name.clone();
		if digblock_net::protocol::validate_name(&name).is_err() {
			n.checked = None;
			return;
		}
		if n.checking.is_some() {
			return;
		}
		n.checking = Some(name.clone());
		self.pending_cmds.push(MenuCommand::CheckName { name });
	}

	/// An availability answer landed. Clears the in-flight slot and, if the
	/// text moved on while it was out, asks once more about what is there
	/// now — the tail of a burst, which is the answer the player is waiting
	/// to see.
	fn name_answered(&mut self, name: String, available: bool, reason: Option<String>) {
		let Some(Screen::ChooseName(n)) = &mut self.screen else {
			return;
		};
		n.checked = Some((name, available, reason));
		n.checking = None;
		let stale = n
			.checked
			.as_ref()
			.is_some_and(|(asked, ..)| *asked != n.name);
		if stale {
			self.check_typed_name();
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

	fn open_browser(&mut self) {
		self.pending_cmds.push(MenuCommand::RefreshServers);
		self.screen = Some(Screen::Browser(BrowserState {
			rows: Vec::new(),
			sel: 0,
			status: None,
			fetching: true,
		}));
	}

	/// The name picker. A rename starts from the current name (so a small
	/// correction is a couple of keystrokes); a first pick starts empty.
	fn open_name_screen(&mut self, rename: bool) {
		let name = if rename {
			self.account
				.as_ref()
				.and_then(|a| a.name.clone())
				.unwrap_or_default()
		} else {
			String::new()
		};
		self.screen = Some(Screen::ChooseName(NameState {
			name,
			checked: None,
			checking: None,
			status: None,
			claiming: false,
			rename,
		}));
	}

	fn activate(&mut self, act: Act) {
		match act {
			Act::CloseMenu => {
				self.entered_world = true;
				self.screen = None;
			}
			// Multiplayer routes through whatever the account still needs:
			// sign in, then pick a name, then the server list. Each stage
			// hands on to the next when its answer lands, so a first-time
			// player presses one button and is walked the rest of the way.
			Act::OpenMultiplayer => match &self.account {
				None => self.activate(Act::StartSignIn {
					then_multiplayer: true,
				}),
				Some(a) if a.name.is_none() => self.open_name_screen(false),
				Some(_) => self.open_browser(),
			},
			Act::StartSignIn { then_multiplayer } => {
				self.pending_cmds.push(MenuCommand::SignIn);
				self.screen = Some(Screen::SignIn(SignInState {
					status: None,
					then_multiplayer,
				}));
			}
			Act::CancelSignIn => {
				self.pending_cmds.push(MenuCommand::CancelSignIn);
				self.screen = Some(Screen::Root { sel: 0 });
			}
			Act::ConfirmName => {
				let Some(Screen::ChooseName(n)) = &mut self.screen else {
					return;
				};
				if n.claiming {
					return;
				}
				let name = n.name.trim().to_string();
				if let Err(e) = digblock_net::protocol::validate_name(&name) {
					n.status = Some(e.into());
					return;
				}
				n.claiming = true;
				n.status = None;
				self.pending_cmds.push(MenuCommand::SetName { name });
			}
			Act::OpenAccount => self.screen = Some(Screen::Account { sel: 0 }),
			Act::CloseAccount => self.screen = Some(Screen::Root { sel: 0 }),
			Act::CopyAccountId => {
				if let Some(a) = &self.account {
					self.wants_copy = Some(a.id.clone());
				}
			}
			Act::OpenRename => self.open_name_screen(true),
			Act::SignOut => {
				self.pending_cmds.push(MenuCommand::SignOut);
				self.account = None;
				self.screen = Some(Screen::Root { sel: 0 });
			}
			Act::LeaveServer => {
				self.wants_disconnect = true;
				self.screen = None;
			}
			Act::LeaveWorld => {
				self.wants_leave_world = true;
				self.screen = None;
			}
			Act::OpenSpellbook => {
				self.wants_spellbook = true;
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
			Act::OpenHiscores => {
				self.wants_hiscores = true;
				self.screen = None;
			}
			Act::Refresh => {
				if let Some(Screen::Browser(b)) = &mut self.screen
					&& !b.fetching
				{
					b.fetching = true;
					b.status = None;
					self.pending_cmds.push(MenuCommand::RefreshServers);
				}
			}
			Act::ToggleFullscreen => self.wants_fullscreen = true,
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
			// No name is chosen here any more — the master puts the
			// account's own name on the token. Reaching this screen at all
			// means we are signed in and named, because Act::OpenMultiplayer
			// walks those stages first.
			Act::JoinRow(i) => {
				let Some(Screen::Browser(mut prev)) = self.screen.take() else {
					return;
				};
				let Some((server_id, server_name, compat, ranked)) = prev.rows.get(i).map(|r| {
					(
						r.entry.id.clone(),
						r.entry.name.clone(),
						r.entry.compat(),
						r.entry.reports_skills,
					)
				}) else {
					self.screen = Some(Screen::Browser(prev));
					return;
				};
				// `acts` does not offer such a row, so this is the scripted
				// path (`--join`): say so rather than spending a one-time
				// token and a QUIC connect on a refusal.
				if let Some(why) = compat_label(compat) {
					prev.status = Some(format!("{server_name}: {why}"));
					self.screen = Some(Screen::Browser(prev));
					return;
				}
				self.join_ranked = Some(ranked);
				self.pending_cmds.push(MenuCommand::Join { server_id });
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
				// (Spellbook, Inventory, Alchemy, Character, Hiscores), then
				// the one exit row, with Settings last. Local and multiplayer read the
				// same in game — Singleplayer and Multiplayer belong to
				// the title menu, and reaching either from inside a world
				// now goes through Leave.
				if self.entered_world {
					acts.push((RowId::RootItem(0), Act::CloseMenu));
					acts.push((RowId::RootItem(3), Act::OpenSpellbook));
					acts.push((RowId::RootItem(6), Act::OpenInventory));
					acts.push((RowId::RootItem(7), Act::OpenAlchemy));
					acts.push((RowId::RootItem(9), Act::OpenCharacter));
					acts.push((RowId::RootItem(12), Act::OpenHiscores));
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
						// The Account row sits below Multiplayer once
						// there is an account to look at. Signed out,
						// Multiplayer itself is the way in — a separate
						// "Sign In" row would be two doors to one place.
						if self.account.is_some() {
							acts.push((RowId::RootItem(10), Act::OpenAccount));
						}
					}
				}
				if self.fullscreen_available {
					acts.push((RowId::RootItem(11), Act::ToggleFullscreen));
				}
				acts.push((RowId::RootItem(4), Act::OpenSettings));
			}
			Some(Screen::Browser(b)) => {
				// A row the handshake would refuse stays listed and stops
				// being an offer: selection skips it, a click on it is dead.
				for (i, row) in b.rows.iter().enumerate() {
					if row.entry.compat() == Compat::Joinable {
						acts.push((RowId::Server(i), Act::JoinRow(i)));
					}
				}
				acts.push((RowId::Refresh, Act::Refresh));
			}
			Some(Screen::Settings { .. }) => {
				for (i, s) in SETTINGS_ROWS.iter().enumerate() {
					// Skipped in lockstep with `view()` — see Setting::TouchJump.
					if matches!(s, Setting::TouchJump) && !self.touch {
						continue;
					}
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
			Some(Screen::SignIn(_)) => {
				acts.push((RowId::SignInCancel, Act::CancelSignIn));
			}
			Some(Screen::ChooseName(_)) => {
				acts.push((RowId::NameField, Act::ConfirmName));
			}
			Some(Screen::Account { .. }) => {
				for (i, act) in ACCOUNT_ROWS.iter().enumerate() {
					acts.push((RowId::AccountRow(i as u8), *act));
				}
				acts.push((RowId::AccountRow(ACCOUNT_BACK), Act::CloseAccount));
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

/// The Account screen's actions, in screen order. Same positional
/// discipline as `SETTINGS_ROWS`: `acts()` walks this and `view()` must
/// emit the rows in the same order, because the selection index is a
/// position into the list.
const ACCOUNT_ROWS: [Act; 3] = [Act::CopyAccountId, Act::OpenRename, Act::SignOut];
const ACCOUNT_BACK: u8 = ACCOUNT_ROWS.len() as u8;

// Render scale is deliberately ABSENT: it is locked at native for
// players and only `--render-scale` moves it (settings::RENDER_SCALE_MIN
// says why). Re-exposing it means a row here, a match arm in
// `adjust_setting` and `set_setting_frac`, a `RowSpec` below, and
// renumbering every row after it — the indices are positional.
const SETTINGS_ROWS: [Setting; 8] = [
	Setting::DrawDist,
	// Lighting detail sits next to draw distance: both trade picture for
	// frame time, and a player hunting fps should find them together
	// rather than one of them filed under "interface" — and the light cap
	// is a third member of the same family, so it sits with them.
	Setting::GiScale,
	Setting::MaxLights,
	Setting::UiScale,
	Setting::MusicVol,
	Setting::SfxVol,
	Setting::Ssr,
	Setting::TouchJump,
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

/// The right column's word for a row the handshake would refuse, and the
/// status line's when a scripted join names one. A menu label, so it lives
/// with the layout; `None` is a row that draws its ping instead.
fn compat_label(c: Compat) -> Option<&'static str> {
	match c {
		Compat::Joinable => None,
		Compat::PeerOlder => Some("server too old"),
		Compat::PeerNewer => Some("client too old"),
	}
}

/// Player-facing name for a GI divisor. Named for the PICTURE rather than
/// the ratio ("Half" would read as half of the window, which it is not —
/// it is half of the render scale, itself already a fraction).
fn gi_scale_label(div: u32) -> &'static str {
	match div {
		// Lever-only (settings::GI_SCALE_DIV_FULL) — the slider cannot
		// reach it, but a capture lever's interactive run must read true.
		1 => "Full",
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
	/// Columns after the text — see `widgets::RowContent::cells`.
	cells: Vec<(f32, String, u32)>,
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
			cells: Vec::new(),
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
pub(crate) struct PlacedRow {
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
	/// Columns after the text.
	pub cells: Vec<(f32, String, u32)>,
}

pub(crate) struct MenuView {
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
	/// panel backdrop, then rows through the framework's row widget. The
	/// dim and the panel go on `Layer::Backdrop`, as every screen's do.
	pub(crate) fn emit(
		&self,
		dl: &mut crate::ui::DrawList,
		style: &crate::ui::Style,
		size: (u32, u32),
		daylight: f32,
	) {
		use crate::ui::{Layer, Rect, theme, widgets};
		dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(theme::SCREEN_DIM, daylight),
		);
		widgets::panel(dl, Layer::Backdrop, self.panel.into(), theme::PANEL_BG);
		for row in &self.rows {
			let cells: Vec<widgets::Cell<'_>> = row
				.cells
				.iter()
				.map(|(x, text, color)| widgets::Cell {
					x: *x,
					text,
					color: *color,
				})
				.collect();
			widgets::row(
				dl,
				style,
				Layer::Content,
				row.rect.into(),
				widgets::RowContent {
					text: &row.text,
					right: row
						.right
						.as_ref()
						.map(|(s, c)| widgets::RowRight::Text(s.as_str(), *c)),
					bar: row.bar,
					color: row.color,
					highlight: row.highlight,
					caret: row.caret,
					cells: &cells,
				},
			);
		}
	}
}

#[cfg(test)]
mod tests;
