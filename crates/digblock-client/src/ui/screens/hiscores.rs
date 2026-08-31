//! The hiscores screen (H): one page of one of the master's boards —
//! overall or a skill — with the account's own row marked. Its only
//! controls pick a board or turn a page; the ENGINE turns those into a
//! fetch through the menu's service channel, so the page does no I/O,
//! and the launcher alone decides which row is "me" (it holds the
//! session; the engine never sees an account id).

use digblock_core::skills::Skill;
use digblock_platform::{InputEvent, Key, MouseButton};

use crate::hotbar::HotbarView;
use crate::menu::{BOARD_PAGE_ROWS, BoardRow, BoardView, MenuCommand};
use crate::ui::draw::Layer;
use crate::ui::widgets::{self, Cell, DIM, Style, Ui, WHITE};
use crate::ui::{HitMap, InvId, Rect, hud, scale, theme};

use super::character::group;

/// What a pointer or a key asked of the page, applied by [`BoardModel::apply`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HiscoresAction {
	/// A tab: `None` is the overall board. The current tab again is a refresh.
	SelectBoard(Option<Skill>),
	/// The neighbouring tab, wrapping at either end.
	StepBoard(i8),
	/// The neighbouring page, clamped to the board's extent.
	StepPage(i8),
}

/// The tabs in order: overall first, then the skills as the roster lists them.
const TABS: usize = Skill::COUNT + 1;

fn board_at(tab: usize) -> Option<Skill> {
	tab.checked_sub(1).map(|i| Skill::ALL[i % Skill::COUNT])
}

fn tab_of(board: Option<Skill>) -> usize {
	match board {
		None => 0,
		Some(s) => 1 + Skill::ALL.iter().position(|&k| k == s).unwrap_or(0),
	}
}

/// What the engine holds for the page: the board and page being looked at, the last answer, and
/// whether one is on its way. Pure: [`BoardModel::request`] hands back the command and the engine
/// sends it. An answer is shown only when it names the board and page in view — a stale one for
/// the tab the player just left must not be drawn under the tab they moved to.
pub struct BoardModel {
	skill: Option<Skill>,
	page: u32,
	view: Option<BoardView>,
	pending: bool,
	error: Option<String>,
}

impl Default for BoardModel {
	fn default() -> Self {
		Self {
			skill: None,
			page: 1,
			view: None,
			pending: false,
			error: None,
		}
	}
}

impl BoardModel {
	/// The fetch for what is in view; marks it outstanding.
	pub fn request(&mut self) -> MenuCommand {
		self.pending = true;
		self.error = None;
		MenuCommand::FetchBoard {
			skill: self.skill,
			page: self.page,
		}
	}

	/// The launcher's answer. A page for some other board or page is dropped — the player moved
	/// on before it landed, and the fetch for where they are is still outstanding.
	pub fn answer(&mut self, result: Result<BoardView, String>) {
		match result {
			Ok(view) => {
				if view.skill == self.skill && view.page == self.page {
					self.view = Some(view);
					self.pending = false;
				}
			}
			Err(e) => {
				self.error = Some(e);
				self.pending = false;
			}
		}
	}

	/// Apply a page action; answers whether what is in view changed, which is the engine's cue to
	/// fetch. Paging past the board's ends and stepping with nothing loaded are no-ops.
	pub fn apply(&mut self, act: HiscoresAction) -> bool {
		match act {
			HiscoresAction::SelectBoard(skill) => {
				self.skill = skill;
				self.page = 1;
				true
			}
			HiscoresAction::StepBoard(d) => {
				let tab = (tab_of(self.skill) as i32 + i32::from(d)).rem_euclid(TABS as i32);
				self.skill = board_at(tab as usize);
				self.page = 1;
				true
			}
			HiscoresAction::StepPage(d) if d < 0 => {
				if self.page > 1 {
					self.page -= 1;
					true
				} else {
					false
				}
			}
			HiscoresAction::StepPage(_) => match self.shown().map(last_page) {
				Some(last) if self.page < last => {
					self.page += 1;
					true
				}
				_ => false,
			},
		}
	}

	/// What a capture stands in for the master with: the answer, already in view.
	pub fn debug_fill(&mut self, view: BoardView) {
		self.skill = view.skill;
		self.page = view.page;
		self.view = Some(view);
		self.pending = false;
		self.error = None;
	}

	/// The page whose SHAPE to lay out while the one in view is on its way: the last answer of
	/// any board, so a tab change keeps the panel's height and columns instead of collapsing to a
	/// status line and springing back when the answer lands. Its figures are never drawn.
	fn placeholder(&self, has_master: bool) -> Option<&BoardView> {
		(has_master && self.pending)
			.then_some(self.view.as_ref())
			.flatten()
	}

	/// The answer for the board and page in view, if it has landed.
	fn shown(&self) -> Option<&BoardView> {
		self.view
			.as_ref()
			.filter(|v| v.skill == self.skill && v.page == self.page)
	}
}

/// The last page of a board, from the total and the size the master actually cut — an empty
/// board still has a page one.
fn last_page(v: &BoardView) -> u32 {
	v.total.div_ceil(v.page_size.max(1)).max(1)
}

/// The board a capture or a sweep shows, since neither has a master to ask: a full page with a
/// second page behind it, so the pager draws one live and one dead end, and one row that is ours.
pub fn sample_board(skill: Option<Skill>) -> BoardView {
	const NAMES: [&str; 10] = [
		"Brookside",
		"Ada",
		"Kestrel",
		"Morrow",
		"Tamsin",
		"Ferrous",
		"Quill",
		"Halden",
		"Ines",
		"Wren",
	];
	let rows = NAMES
		.iter()
		.enumerate()
		.map(|(i, name)| {
			let level = 60 - 3 * i as u32;
			BoardRow {
				rank: i as u32 + 1,
				name: (*name).to_string(),
				level,
				xp: digblock_core::skills::total_xp_for(level) + 137 * (10 - i as u64),
				me: i == 3,
			}
		})
		.collect();
	BoardView {
		skill,
		page: 1,
		page_size: BOARD_PAGE_ROWS,
		total: 37,
		rows,
	}
}

enum Row {
	Title,
	Space,
	/// The board tabs, one button each.
	Tabs,
	/// The column captions over the entries.
	Head,
	/// One ranked account: an index into [`HiscoresView::entries`].
	Entry(usize),
	/// Dim detail line (a status, an empty board).
	Info(String),
	/// Previous, the page summary, next.
	Pager,
}

/// One entry's four columns as text — rank, name, level, XP — so the
/// layout pass measures the same strings the emit pass draws.
struct Entry {
	cols: [String; 4],
	me: bool,
}

/// The page's per-frame view, derived once from the engine's [`BoardModel`].
pub struct HiscoresView {
	skill: Option<Skill>,
	rows: Vec<Row>,
	entries: Vec<Entry>,
	/// The entries are the previous page's, kept for the layout alone:
	/// measured, never drawn, until the page in view lands.
	loading: bool,
	/// The pager's summary and whether each of its ends is live.
	summary: String,
	prev: bool,
	next: bool,
}

impl HiscoresView {
	/// `has_master` is whether the launcher wired services at all — without a master URL there
	/// is nobody to ask, and the page says so instead of waiting forever.
	pub fn build(model: &BoardModel, has_master: bool) -> Self {
		let mut rows = vec![Row::Title, Row::Space, Row::Tabs, Row::Space];
		let mut entries = Vec::new();
		let (mut summary, mut prev, mut next) = (String::new(), false, false);
		let loading = model.shown().is_none();
		match model.shown().or_else(|| model.placeholder(has_master)) {
			Some(v) => {
				if v.rows.is_empty() {
					rows.push(Row::Info("Nobody here yet".into()));
				} else {
					rows.push(Row::Head);
					for (i, r) in v.rows.iter().enumerate() {
						entries.push(Entry {
							cols: [
								group(u64::from(r.rank)),
								r.name.clone(),
								r.level.to_string(),
								group(r.xp),
							],
							me: r.me,
						});
						rows.push(Row::Entry(i));
					}
				}
				if loading {
					summary = "Fetching...".into();
				} else {
					let last = last_page(v);
					summary = format!(
						"Page {} of {} - {} ranked",
						v.page,
						last,
						group(u64::from(v.total))
					);
					prev = v.page > 1;
					next = v.page < last;
				}
				rows.push(Row::Space);
				rows.push(Row::Pager);
			}
			None => rows.push(Row::Info(if !has_master {
				"No master server; run with --master-url".into()
			} else if let Some(e) = &model.error {
				format!("Hiscores failed: {e}")
			} else {
				"Fetching...".into()
			})),
		}
		Self {
			skill: model.skill,
			rows,
			entries,
			loading,
			summary,
			prev,
			next,
		}
	}

	/// The column captions: the overall board's figures are sums.
	fn head(&self) -> [&'static str; 4] {
		match self.skill {
			None => ["Rank", "Name", "Total Level", "Total XP"],
			Some(_) => ["Rank", "Name", "Level", "XP"],
		}
	}

	/// Where the three columns after the rank start, past the row's text
	/// origin: each sits one gap past the widest entry of the one before,
	/// captions included, so a run of rows lines up.
	fn col_x(&self, style: &Style) -> [f32; 3] {
		let head = self.head();
		let widest = |c: usize| {
			self.entries
				.iter()
				.map(|e| style.measure(&e.cols[c]))
				.fold(style.measure(head[c]), f32::max)
		};
		let gap = theme::SECTION_GAP * style.scale;
		let mut x = [0.0f32; 3];
		let mut run = widest(0) + gap;
		for (i, slot) in x.iter_mut().enumerate() {
			*slot = run;
			run += widest(i + 1) + gap;
		}
		x
	}

	fn cells<'a>(&self, texts: &'a [&'a str], col_x: &[f32; 3], color: u32) -> Vec<Cell<'a>> {
		texts
			.iter()
			.skip(1)
			.zip(col_x)
			.map(|(text, &x)| Cell { x, text, color })
			.collect()
	}
}

fn tab_label(board: Option<Skill>) -> String {
	match board {
		None => "Overall".into(),
		Some(s) => s.name().into(),
	}
}

const PREV: &str = "Prev";
const NEXT: &str = "Next";

fn box_h(row: &Row, scale: f32) -> f32 {
	match row {
		Row::Space => 0.0,
		_ => theme::ROW_H * scale,
	}
}

fn row_h(row: &Row, scale: f32) -> f32 {
	box_h(row, scale)
		+ match row {
			Row::Space => theme::SECTION_GAP * scale,
			_ => 0.0,
		}
}

/// A button's own width: its label plus the text pad either side.
fn button_w(style: &Style, text: &str) -> f32 {
	theme::text_row_w(style, text)
}

fn measure_rows(view: &HiscoresView, style: &Style) -> (f32, f32) {
	let s = style.scale;
	let col_x = view.col_x(style);
	let gap = theme::ROW_GAP * s;
	let (mut width, mut height) = (0.0f32, 0.0f32);
	for row in &view.rows {
		let w = match row {
			Row::Space => 0.0,
			Row::Title => theme::text_row_w(style, "Hiscores"),
			Row::Info(t) => theme::text_row_w(style, t),
			Row::Tabs => {
				let widest = (0..TABS)
					.map(|t| button_w(style, &tab_label(board_at(t))))
					.fold(0.0, f32::max);
				widest * TABS as f32 + gap * (TABS - 1) as f32
			}
			Row::Head => {
				let head = view.head();
				let cells = view.cells(&head, &col_x, DIM);
				2.0 * scale::text_pad_x(s)
					+ style.measure(head[0]).max(widgets::cells_w(style, &cells))
			}
			Row::Entry(i) => {
				let e = &view.entries[*i];
				let texts: Vec<&str> = e.cols.iter().map(String::as_str).collect();
				let cells = view.cells(&texts, &col_x, WHITE);
				2.0 * scale::text_pad_x(s)
					+ style.measure(texts[0]).max(widgets::cells_w(style, &cells))
			}
			Row::Pager => {
				button_w(style, PREV)
					+ button_w(style, NEXT)
					+ theme::text_row_w(style, &view.summary)
					+ 2.0 * gap
			}
		};
		width = width.max(w);
		height += row_h(row, s);
	}
	(width, height)
}

#[derive(Default)]
pub struct HiscoresScreen {
	open: bool,
	/// Where the pointer is: for the buttons' hover, and to name the
	/// hotbar slot underneath, which every screen drawn over the bar owes.
	cursor: (f32, f32),
}

impl HiscoresScreen {
	pub fn is_open(&self) -> bool {
		self.open
	}

	pub fn open(&mut self) {
		self.open = true;
	}

	pub fn close(&mut self) {
		self.open = false;
	}

	/// Clicks resolve against the hit map the last `build` registered —
	/// the frame-behind convention every screen shares. The arrows turn
	/// pages and switch boards; the closing keys are the ENGINE's.
	pub fn handle_input(
		&mut self,
		event: &InputEvent,
		hits: &HitMap<InvId>,
	) -> Option<HiscoresAction> {
		match *event {
			InputEvent::CursorMoved { x, y } => {
				self.cursor = (x, y);
				None
			}
			InputEvent::MouseButton {
				button: MouseButton::Left,
				pressed: true,
			} => match hits.hit(self.cursor.0, self.cursor.1)? {
				InvId::Board(tab) => Some(HiscoresAction::SelectBoard(board_at(tab as usize))),
				InvId::BoardPrev => Some(HiscoresAction::StepPage(-1)),
				InvId::BoardNext => Some(HiscoresAction::StepPage(1)),
				_ => None,
			},
			InputEvent::Key {
				key: Key::Left,
				pressed: true,
			} => Some(HiscoresAction::StepPage(-1)),
			InputEvent::Key {
				key: Key::Right,
				pressed: true,
			} => Some(HiscoresAction::StepPage(1)),
			InputEvent::Key {
				key: Key::Up,
				pressed: true,
			} => Some(HiscoresAction::StepBoard(-1)),
			InputEvent::Key {
				key: Key::Down,
				pressed: true,
			} => Some(HiscoresAction::StepBoard(1)),
			_ => None,
		}
	}

	/// Emit the page: the shared dim, the shared panel, the rows. The
	/// character screen's two-pass shape — a page is a fixed handful of
	/// rows (`BOARD_PAGE_ROWS` plus its furniture), so no fit ladder.
	pub fn build(
		&self,
		ui: &mut Ui<'_, InvId>,
		size: (u32, u32),
		view: &HiscoresView,
		bar: &HotbarView,
		daylight: f32,
	) {
		let s = ui.style.scale;
		ui.dl.quad(
			Layer::Backdrop,
			Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32),
			widgets::scaled_dim(theme::SCREEN_DIM, daylight),
		);
		let pad = theme::PAD * s;
		let gap = theme::ROW_GAP * s;
		let (width, height) = measure_rows(view, &ui.style);
		let row_w = width
			.max(theme::MIN_PANEL_W * s)
			.min(size.0 as f32 - 4.0 * pad);
		let row_x = (size.0 as f32 - row_w) / 2.0;
		let bar_top = hud::row_grid(size, s).origin.1;
		let row_y = (bar_top - theme::GAP_ABOVE_BAR * s - (height + 2.0 * pad))
			.max(theme::TOP_MARGIN * s)
			+ pad;
		let panel = theme::panel_of(row_x, row_y, row_w, height, s);
		widgets::panel(ui.dl, Layer::Backdrop, panel, theme::PANEL_BG);

		let col_x = view.col_x(&ui.style);
		let hovered = |r: Rect| r.contains(self.cursor.0, self.cursor.1);
		let mut y = row_y;
		for row in &view.rows {
			let rect = Rect::new(row_x, y, row_w, box_h(row, s));
			y += row_h(row, s);
			match row {
				Row::Space => {}
				Row::Title | Row::Info(_) => {
					let (text, color) = match row {
						Row::Info(t) => (t.as_str(), DIM),
						_ => ("Hiscores", WHITE),
					};
					widgets::row(
						ui.dl,
						&ui.style,
						Layer::Content,
						rect,
						widgets::RowContent {
							text,
							right: None,
							bar: None,
							color,
							highlight: false,
							caret: false,
							cells: &[],
						},
					);
				}
				Row::Tabs => {
					let w = (rect.w - gap * (TABS - 1) as f32) / TABS as f32;
					for tab in 0..TABS {
						let r = Rect::new(rect.x + (w + gap) * tab as f32, rect.y, w, rect.h);
						let board = board_at(tab);
						// The current tab wears the wash a hover paints:
						// it is the one the pointer would leave alone.
						let current = board == view.skill;
						widgets::button(
							ui,
							InvId::Board(tab as u8),
							r,
							&tab_label(board),
							true,
							current || hovered(r),
						);
					}
				}
				Row::Head => {
					let head = view.head();
					let cells = view.cells(&head, &col_x, DIM);
					widgets::row(
						ui.dl,
						&ui.style,
						Layer::Content,
						rect,
						widgets::RowContent {
							text: head[0],
							right: None,
							bar: None,
							color: DIM,
							highlight: false,
							caret: false,
							cells: &cells,
						},
					);
				}
				// A loading page keeps its rows' room and shows none of them.
				Row::Entry(_) if view.loading => {}
				Row::Entry(i) => {
					let e = &view.entries[*i];
					let texts: Vec<&str> = e.cols.iter().map(String::as_str).collect();
					let cells = view.cells(&texts, &col_x, WHITE);
					widgets::row(
						ui.dl,
						&ui.style,
						Layer::Content,
						rect,
						widgets::RowContent {
							text: texts[0],
							right: None,
							bar: None,
							color: WHITE,
							// Our own row wears the wash: the one number
							// the player came to find.
							highlight: e.me,
							caret: false,
							cells: &cells,
						},
					);
				}
				Row::Pager => {
					let prev = Rect::new(rect.x, rect.y, button_w(&ui.style, PREV), rect.h);
					let next_w = button_w(&ui.style, NEXT);
					let next = Rect::new(rect.x + rect.w - next_w, rect.y, next_w, rect.h);
					widgets::button(ui, InvId::BoardPrev, prev, PREV, view.prev, hovered(prev));
					widgets::button(ui, InvId::BoardNext, next, NEXT, view.next, hovered(next));
					let px = ui.style.glyph;
					let w = ui.style.measure(&view.summary);
					ui.dl.text(
						Layer::Content,
						rect.x + (rect.w - w) / 2.0,
						rect.y + (rect.h - px) / 2.0,
						px,
						DIM,
						&view.summary,
					);
				}
			}
		}
		// Drawn OVER the bar, which draws after it at the window's scale —
		// so naming its slots is this page's job, at this frame's glyph.
		if let Some((anchor, name)) = hud::hovered_bar_slot(size, s, bar, self.cursor) {
			widgets::tooltip(ui.dl, &ui.style, size, anchor, &name);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::draw::{Cmd, DrawList};

	const SIZE: (u32, u32) = (1280, 800);

	fn empty_bar() -> HotbarView {
		HotbarView::build(
			&digblock_core::hotbar::Hotbar::new(),
			&digblock_core::inventory::Inventory::default(),
			&crate::hotbar::ToolSwap::default(),
		)
	}

	fn emit(model: &BoardModel, has_master: bool) -> (DrawList, HitMap<InvId>) {
		let mut dl = DrawList::default();
		let mut hits: HitMap<InvId> = HitMap::default();
		let mut screen = HiscoresScreen::default();
		screen.open();
		screen.build(
			&mut Ui {
				dl: &mut dl,
				hits: &mut hits,
				style: Style::new(1.0),
			},
			SIZE,
			&HiscoresView::build(model, has_master),
			&empty_bar(),
			1.0,
		);
		(dl, hits)
	}

	fn texts(dl: &DrawList) -> Vec<String> {
		let (cmds, arena) = dl.cmds(Layer::Content);
		cmds.iter()
			.filter_map(|c| match c {
				Cmd::Text { range, .. } => {
					Some(arena[range.0 as usize..range.1 as usize].to_string())
				}
				_ => None,
			})
			.collect()
	}

	/// A model showing `view`, the way a landed answer leaves it.
	fn showing(view: BoardView) -> BoardModel {
		let mut m = BoardModel::default();
		m.apply(HiscoresAction::SelectBoard(view.skill));
		for _ in 1..view.page {
			m.page += 1;
		}
		m.answer(Ok(view));
		m
	}

	/// Every figure on the page is the answer's, and the pager is derived from its total and the
	/// size the master CUT — never the size asked for. Each end of the pager is a hit only where
	/// there is a page to turn to, and every tab is always one.
	#[test]
	fn the_page_and_its_pager_derive_from_the_answer() {
		let mut view = sample_board(Some(Skill::Mining));
		// A master that cut a shorter page than asked for: the pager
		// divides by what came back.
		view.page_size = 4;
		let last = view.total.div_ceil(view.page_size);
		assert!(last > 1, "the sample has pages behind it");
		let (dl, hits) = emit(&showing(view.clone()), true);
		let all = texts(&dl).join("\n");
		for r in &view.rows {
			for want in [
				group(u64::from(r.rank)),
				r.name.clone(),
				r.level.to_string(),
				group(r.xp),
			] {
				assert!(all.contains(&want), "{want:?} missing from:\n{all}");
			}
		}
		assert!(all.contains(&format!("Page 1 of {last}")), "{all}");
		for tab in 0..TABS {
			assert!(hits.rect_of(InvId::Board(tab as u8)).is_some(), "tab {tab}");
		}
		assert!(hits.rect_of(InvId::BoardNext).is_some(), "a next page");
		assert!(
			hits.rect_of(InvId::BoardPrev).is_none(),
			"no page before the first"
		);

		// The last page: the ends swap.
		let mut end = view.clone();
		end.page = last;
		let (_, hits) = emit(&showing(end), true);
		assert!(
			hits.rect_of(InvId::BoardNext).is_none(),
			"nothing past the last"
		);
		assert!(hits.rect_of(InvId::BoardPrev).is_some());

		// Skill and overall boards caption their figures differently.
		assert!(all.contains("Level") && !all.contains("Total Level"));
		let (dl, _) = emit(&showing(sample_board(None)), true);
		assert!(texts(&dl).join("\n").contains("Total Level"));
	}

	/// A tab change keeps the panel the previous page's size until the new page lands — the
	/// same rows and columns are measured — while none of the previous page's figures are drawn
	/// under the new tab. The first fetch, with nothing before it, still says so in one line.
	#[test]
	fn a_pending_tab_keeps_the_previous_pages_shape_and_shows_none_of_it() {
		let mut m = showing(sample_board(None));
		let style = Style::new(1.0);
		let before = measure_rows(&HiscoresView::build(&m, true), &style);
		assert!(m.apply(HiscoresAction::SelectBoard(Some(Skill::Mining))));
		let _ = m.request();
		let view = HiscoresView::build(&m, true);
		assert_eq!(measure_rows(&view, &style), before, "the panel moved");
		let (dl, hits) = emit(&m, true);
		let all = texts(&dl).join("\n");
		for r in &sample_board(None).rows {
			assert!(
				!all.contains(&r.name),
				"{} drawn under the new tab:\n{all}",
				r.name
			);
		}
		assert!(all.contains("Fetching"), "{all}");
		assert!(
			hits.rect_of(InvId::BoardNext).is_none(),
			"nothing to page while loading"
		);
		// Landed: the figures return and the pager wakes.
		m.answer(Ok(sample_board(Some(Skill::Mining))));
		let (dl, hits) = emit(&m, true);
		assert!(
			texts(&dl)
				.join("\n")
				.contains(&sample_board(None).rows[0].name)
		);
		assert!(hits.rect_of(InvId::BoardNext).is_some());
		// Nothing before it: one line, no phantom table.
		let mut fresh = BoardModel::default();
		let _ = fresh.request();
		assert!(measure_rows(&HiscoresView::build(&fresh, true), &style).1 < before.1);
	}

	/// Our own row is the one that wears the highlight wash — one wash per
	/// `me` row, plus the current tab's.
	#[test]
	fn the_own_row_wears_the_wash() {
		let view = sample_board(None);
		let mine = view.rows.iter().filter(|r| r.me).count();
		assert!(mine > 0, "the sample marks a row as ours");
		let (dl, _) = emit(&showing(view), true);
		let washes = dl
			.cmds(Layer::Backdrop)
			.0
			.iter()
			.filter(|c| matches!(c, Cmd::Quad { color, .. } if *color == widgets::ROW_HILIGHT))
			.count();
		assert_eq!(washes, mine + 1);
	}

	/// An answer for a board or page the player has left is dropped rather than drawn under the
	/// tab they moved to; the one for where they are lands. A failure clears the wait so the page
	/// can say so.
	#[test]
	fn a_stale_answer_never_lands_on_the_wrong_tab() {
		let mut m = BoardModel::default();
		assert!(matches!(
			m.request(),
			MenuCommand::FetchBoard {
				skill: None,
				page: 1
			}
		));
		m.answer(Ok(sample_board(Some(Skill::Arcana))));
		assert!(m.shown().is_none(), "another board's page was shown");
		let (dl, _) = emit(&m, true);
		assert!(texts(&dl).join("\n").contains("Fetching"), "still waiting");
		m.answer(Ok(sample_board(None)));
		assert!(m.shown().is_some());

		let mut m = BoardModel::default();
		m.request();
		m.answer(Err("down".into()));
		let (dl, _) = emit(&m, true);
		assert!(
			texts(&dl).join("\n").contains("down"),
			"the failure is shown"
		);
		let (dl, _) = emit(&BoardModel::default(), false);
		assert!(texts(&dl).join("\n").contains("master"), "no master to ask");
	}

	/// Paging stops at the board's ends and needs a landed page to go forward from; the tabs
	/// wrap; and each move that changes the view asks for a fetch, while a blocked one asks for
	/// nothing.
	#[test]
	fn paging_is_clamped_and_tabs_wrap() {
		let mut m = BoardModel::default();
		assert!(
			!m.apply(HiscoresAction::StepPage(-1)),
			"before the first page"
		);
		assert!(
			!m.apply(HiscoresAction::StepPage(1)),
			"nothing loaded to page from"
		);
		let view = sample_board(None);
		let last = last_page(&view);
		m.answer(Ok(view));
		for p in 2..=last {
			assert!(m.apply(HiscoresAction::StepPage(1)));
			assert_eq!(m.page, p);
			// The page is gone until its answer lands, and the next
			// step has nothing to go forward from.
			assert!(m.shown().is_none());
			let mut landed = sample_board(None);
			landed.page = p;
			m.answer(Ok(landed));
		}
		assert!(!m.apply(HiscoresAction::StepPage(1)), "past the last page");
		assert!(m.apply(HiscoresAction::StepPage(-1)));

		let mut seen = Vec::new();
		for _ in 0..TABS {
			assert!(m.apply(HiscoresAction::StepBoard(1)));
			assert_eq!(m.page, 1, "a new board opens at its first page");
			seen.push(m.skill);
		}
		seen.sort_by_key(|s| s.map(|s| s as u8 + 1).unwrap_or(0));
		let mut all: Vec<Option<Skill>> = (0..TABS).map(board_at).collect();
		all.sort_by_key(|s| s.map(|s| s as u8 + 1).unwrap_or(0));
		assert_eq!(seen, all, "stepping walks every tab once");
		assert!(m.apply(HiscoresAction::StepBoard(-1)));
		assert!(
			m.apply(HiscoresAction::SelectBoard(m.skill)),
			"the current tab again is a refresh"
		);
	}

	/// A click lands on what the last frame laid out, and the arrows drive the same actions.
	#[test]
	fn clicks_and_arrows_ask_the_page_actions() {
		let m = showing(sample_board(None));
		let (_, hits) = emit(&m, true);
		let mut screen = HiscoresScreen::default();
		screen.open();
		let click = |screen: &mut HiscoresScreen, id: InvId| {
			let r = hits.rect_of(id).expect("a hit");
			screen.handle_input(
				&InputEvent::CursorMoved {
					x: r.x + r.w / 2.0,
					y: r.y + r.h / 2.0,
				},
				&hits,
			);
			screen.handle_input(
				&InputEvent::MouseButton {
					button: MouseButton::Left,
					pressed: true,
				},
				&hits,
			)
		};
		assert_eq!(
			click(&mut screen, InvId::Board(1)),
			Some(HiscoresAction::SelectBoard(Some(Skill::ALL[0])))
		);
		assert_eq!(
			click(&mut screen, InvId::BoardNext),
			Some(HiscoresAction::StepPage(1))
		);
		let key = |screen: &mut HiscoresScreen, key: Key| {
			screen.handle_input(&InputEvent::Key { key, pressed: true }, &hits)
		};
		assert_eq!(
			key(&mut screen, Key::Left),
			Some(HiscoresAction::StepPage(-1))
		);
		assert_eq!(
			key(&mut screen, Key::Down),
			Some(HiscoresAction::StepBoard(1))
		);
		assert_eq!(key(&mut screen, Key::H), None, "closing is the engine's");
	}
}
