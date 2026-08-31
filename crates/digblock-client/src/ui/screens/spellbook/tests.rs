use super::*;
use crate::ui::draw::{Cmd, DrawList};
use crate::ui::widgets::Style;
use digblock_core::block::Essence;
use digblock_core::skills::{self, Skill, SkillSet};

const SIZE: (u32, u32) = (1280, 800);

struct Rig {
	screen: SpellScreen,
	hits: HitMap<InvId>,
	view: SpellView,
	bar: HotbarView,
	style: Style,
}

/// A bar with nothing on it — these rigs are about the page. The bar's
/// own naming is swept across all four pages in `screens::tests`.
fn empty_bar() -> HotbarView {
	HotbarView::build(
		&digblock_core::hotbar::Hotbar::new(),
		&Inventory::default(),
		&crate::hotbar::ToolSwap::default(),
	)
}

fn skills_at(level: u32) -> SkillSet {
	let mut s = SkillSet::default();
	s.set_total(Skill::Arcana, skills::total_xp_for(level));
	s
}

/// A page at `level` with `stock` in the purse, drawn once so the hit
/// map is the frame's own.
fn rig_at(level: u32, stock: impl Fn(&mut Inventory)) -> Rig {
	rig_with_road(level, 0, stock)
}

/// The same page with a causeway already standing for `road_secs`.
fn rig_with_road(level: u32, road_secs: u32, stock: impl Fn(&mut Inventory)) -> Rig {
	let mut inv = Inventory::default();
	stock(&mut inv);
	let view = SpellView::build(&skills_at(level), 0, &all_ports(), &inv, road_secs);
	let mut r = Rig {
		screen: SpellScreen::default(),
		hits: HitMap::default(),
		view,
		bar: empty_bar(),
		style: Style::new(1.0),
	};
	r.screen.open();
	build(&mut r);
	r
}

fn build(r: &mut Rig) -> DrawList {
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	r.screen.build(
		&mut Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: r.style,
		},
		SIZE,
		&r.view,
		&r.bar,
		&crate::spellbar::SpellBarView::empty(),
		1.0,
	);
	r.hits = hits;
	dl
}

fn hover(r: &mut Rig, at: (f32, f32)) -> DrawList {
	r.screen.handle_input(
		&InputEvent::CursorMoved { x: at.0, y: at.1 },
		&r.hits,
		&r.style,
		&r.view,
		&crate::spellbar::SpellBar::default(),
	);
	build(r)
}

/// Press and release at `at` — a click, in the drag machine's terms
/// (the action resolves on the RELEASE, readiness-gated).
fn click(r: &mut Rig, at: (f32, f32)) -> Option<SpellAction> {
	let bar = crate::spellbar::SpellBar::default();
	r.screen.handle_input(
		&InputEvent::CursorMoved { x: at.0, y: at.1 },
		&r.hits,
		&r.style,
		&r.view,
		&bar,
	);
	r.screen.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		&r.hits,
		&r.style,
		&r.view,
		&bar,
	);
	r.screen.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: false,
		},
		&r.hits,
		&r.style,
		&r.view,
		&bar,
	)
}

fn center(rect: Rect) -> (f32, f32) {
	(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
}

fn quads(dl: &DrawList, layer: Layer) -> Vec<(Rect, u32)> {
	dl.cmds(layer)
		.0
		.iter()
		.filter_map(|c| match c {
			Cmd::Quad { rect, color } => Some((*rect, *color)),
			_ => None,
		})
		.collect()
}

/// Every essence icon the page drew, with the essence it names.
fn essence_icons(dl: &DrawList) -> Vec<(Rect, Essence)> {
	dl.cmds(Layer::Icons)
		.0
		.iter()
		.filter_map(|c| match c {
			Cmd::Icon {
				rect, tex_layer, ..
			} => Essence::ALL
				.iter()
				.find(|e| e.icon_tex() == *tex_layer)
				.map(|e| (*rect, *e)),
			_ => None,
		})
		.collect()
}

/// The ink of every run sharing a row top with `y`.
fn ink_at(dl: &DrawList, y: f32) -> Vec<u32> {
	dl.cmds(Layer::Content)
		.0
		.iter()
		.filter_map(|c| match c {
			Cmd::Text { y: ty, color, .. } if (ty - y).abs() < 0.5 => Some(*color),
			_ => None,
		})
		.collect()
}

fn ghost_texts(dl: &DrawList) -> Vec<String> {
	let (cmds, arena) = dl.cmds(Layer::Ghost);
	cmds.iter()
		.filter_map(|c| match c {
			Cmd::Text { range, .. } => Some(arena[range.0 as usize..range.1 as usize].to_string()),
			_ => None,
		})
		.collect()
}

/// A fare is a chip and never a word: every priced row draws its
/// essence's icon inside the rect the page hit-tests for it, and the
/// number beside it reads short exactly while the purse cannot cover it.
#[test]
fn a_fare_is_a_chip_the_purse_answers_for() {
	let level = digblock_core::unlock::SPHERE_AT;
	let mut poor = rig_at(level, |_| {});
	let dl = build(&mut poor);
	let icons = essence_icons(&dl);
	let fares = poor.view.fares();
	assert!(fares.len() >= 2, "too few priced rows to be a sweep");
	for (slot, essence) in &fares {
		let rect = poor
			.hits
			.rect_of(InvId::SpellFare(*slot))
			.unwrap_or_else(|| panic!("slot {slot} drew no fare chip"));
		let (icon, named) = icons
			.iter()
			.find(|(r, _)| rect.contains(r.x + r.w / 2.0, r.y + r.h / 2.0))
			.unwrap_or_else(|| panic!("slot {slot}: no icon inside its chip"));
		assert_eq!(named, essence, "slot {slot} drew the wrong essence");
		assert!(
			icon.x >= rect.x && icon.x + icon.w <= rect.x + rect.w + 0.01,
			"slot {slot}: the icon is not inside the rect the page hit-tests"
		);
		assert!(
			ink_at(&dl, icon.y).contains(&theme::SHORT_INK),
			"slot {slot}: an unaffordable fare is not in the short ink"
		);
	}
	let mut rich = rig_at(level, |inv| {
		for e in Essence::ALL {
			inv.add_sub(e, 9_999);
		}
	});
	let dl = build(&mut rich);
	let paid = essence_icons(&dl);
	assert_eq!(paid.len(), icons.len(), "the same rows are priced");
	for (icon, _) in &paid {
		assert!(
			!ink_at(&dl, icon.y).contains(&theme::SHORT_INK),
			"an affordable fare still reads short"
		);
	}
}

/// The fare names its essence on hover, like every other tile on every
/// other page — a Ghost box under a Ghost name. The page grew hover with
/// no fixture behind it, so neither screen sweep ever saw one.
#[test]
fn hovering_a_fare_names_its_essence() {
	let mut r = rig_at(digblock_core::unlock::SPHERE_AT, |inv| {
		for e in Essence::ALL {
			inv.add_sub(e, 9_999);
		}
	});
	let mut named = 0;
	for (slot, essence) in r.view.fares() {
		let at = center(r.hits.rect_of(InvId::SpellFare(slot)).expect("a chip"));
		let dl = hover(&mut r, at);
		assert_eq!(
			ghost_texts(&dl),
			vec![essence.name().to_string()],
			"slot {slot}"
		);
		assert_eq!(quads(&dl, Layer::Ghost).len(), 1, "one box behind the name");
		named += 1;
	}
	assert!(named >= 2, "too few priced rows to be a sweep: {named}");
	// A cursor that has left — what a lifted touch sends — names nothing.
	let dl = hover(&mut r, (-1.0, -1.0));
	assert!(
		ghost_texts(&dl).is_empty(),
		"a cursor that left still names"
	);
}

/// The hover wash goes UNDER the fare's icon, not over it. `widgets::row`
/// painted it on the caller's layer, and the whole of `Layer::Content`
/// draws above the whole of `Layer::Icons` — so a hovered row washed its
/// essence icon and left the number beside it crisp, one row reading as
/// two states.
#[test]
fn the_hover_wash_sits_under_the_fare_icon() {
	let mut r = rig_at(digblock_core::unlock::SPHERE_AT, |inv| {
		for e in Essence::ALL {
			inv.add_sub(e, 9_999);
		}
	});
	let (slot, _) = r.view.fares()[0];
	let chip = r.hits.rect_of(InvId::SpellFare(slot)).expect("a chip");
	let dl = hover(&mut r, center(chip));
	let icon = essence_icons(&dl)
		.into_iter()
		.find(|(rect, _)| chip.contains(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0))
		.expect("the hovered fare's icon")
		.0;
	let covers = |rect: &Rect| {
		rect.contains(icon.x + 0.5, icon.y + 0.5)
			&& rect.contains(icon.x + icon.w - 0.5, icon.y + icon.h - 0.5)
	};
	assert!(
		!quads(&dl, Layer::Content)
			.iter()
			.any(|(rect, _)| covers(rect)),
		"a Content quad covers the fare icon"
	);
	assert!(
		quads(&dl, Layer::Backdrop)
			.iter()
			.any(|(rect, c)| covers(rect) && *c == crate::ui::widgets::ROW_HILIGHT),
		"the wash is not behind the icon either"
	);
}

/// A click on the fare acts on the row it belongs to: the chip has its
/// own id so the page can NAME the essence, and that id must not turn the
/// price into a dead patch over a castable spell.
#[test]
fn clicking_a_fare_casts_its_row() {
	let mut r = rig_at(digblock_core::unlock::SPHERE_AT, |inv| {
		for e in Essence::ALL {
			inv.add_sub(e, 9_999);
		}
	});
	let (slot, _) = r.view.fares()[0];
	let row = r.hits.rect_of(InvId::Spell(slot)).expect("a castable row");
	let chip = r.hits.rect_of(InvId::SpellFare(slot)).expect("its chip");
	let on_row = click(&mut r, (row.x + row.w * 0.1, row.y + row.h / 2.0));
	let on_chip = click(&mut r, center(chip));
	assert!(
		matches!(on_row, Some(SpellAction::CastWisp)),
		"the row casts"
	);
	assert!(
		matches!(on_chip, Some(SpellAction::CastWisp)),
		"the fare is not a hole in its own row"
	);
}

/// Click this slot's row and report what it asked for.
fn press_slot(r: &mut Rig, slot: u8) -> Option<SpellAction> {
	let row = r.hits.rect_of(InvId::Spell(slot))?;
	click(r, (row.x + row.w * 0.1, row.y + row.h / 2.0))
}

/// The causeway's row: absent below its rung, priced at it, and — the
/// property most likely to regress — clickable with a road standing and
/// an EMPTY purse, because taking one up costs nothing. Fare and rung
/// both read off the tables.
#[test]
fn the_causeway_row_prices_a_cast_and_never_the_taking_up() {
	let rung = digblock_core::unlock::CAUSEWAY_AT;
	let (essence, fare) = digblock_core::alchemy::causeway_price(rung);

	let mut below = rig_at(rung - 1, |inv| inv.add_sub(essence, fare));
	assert!(
		press_slot(&mut below, SLOT_CAUSEWAY).is_none(),
		"the row stands below its rung"
	);

	let mut broke = rig_at(rung, |_| {});
	assert!(
		press_slot(&mut broke, SLOT_CAUSEWAY).is_none(),
		"an unpayable road casts"
	);

	let mut funded = rig_at(rung, |inv| inv.add_sub(essence, fare));
	assert!(
		matches!(
			press_slot(&mut funded, SLOT_CAUSEWAY),
			Some(SpellAction::ToggleCauseway)
		),
		"a payable road refuses to cast"
	);

	// Standing, and nothing in the purse: the row is the way OUT of a
	// causeway, so it cannot be priced.
	let mut standing = rig_with_road(rung, 1, |_| {});
	assert!(
		matches!(
			press_slot(&mut standing, SLOT_CAUSEWAY),
			Some(SpellAction::ToggleCauseway)
		),
		"a caster cannot take their own road up while broke"
	);
	// And it shows what is left rather than a fare.
	assert!(
		!standing
			.view
			.fares()
			.iter()
			.any(|&(slot, _)| slot == SLOT_CAUSEWAY),
		"a standing road still advertises a price"
	);
}

/// The fares are two columns, not a run of chips: every priced row's
/// icon stands on one vertical and every amount ends on another, so a
/// two-digit fare under a one-digit one does not push its icon left.
/// The sweep counts the amount widths it saw, because a page whose
/// fares all took the same width would line up by accident.
#[test]
fn the_fares_stand_in_two_columns() {
	let mut r = rig_at(digblock_core::unlock::SPHERE_AT, |_| {});
	let dl = build(&mut r);
	let icons = essence_icons(&dl);
	assert!(icons.len() >= 2, "too few priced rows to be a sweep");
	let (cmds, arena) = dl.cmds(Layer::Content);
	let mut widths = std::collections::BTreeSet::new();
	let mut icon_xs = std::collections::BTreeSet::new();
	let mut ends = std::collections::BTreeSet::new();
	for (icon, _) in &icons {
		let amount = cmds
			.iter()
			.filter_map(|c| match c {
				Cmd::Text { x, y, range, .. } if (y - icon.y).abs() < 0.5 && *x > icon.x => {
					Some((*x, &arena[range.0 as usize..range.1 as usize]))
				}
				_ => None,
			})
			.next()
			.expect("an amount to the right of every fare icon");
		let w = r.style.measure(amount.1);
		widths.insert(w.to_bits());
		icon_xs.insert(icon.x.round() as i32);
		ends.insert((amount.0 + w).round() as i32);
	}
	assert!(
		widths.len() >= 2,
		"every fare took the same width: nothing to align"
	);
	assert_eq!(
		icon_xs.len(),
		1,
		"the icons do not share a column: {icon_xs:?}"
	);
	assert_eq!(
		ends.len(),
		1,
		"the amounts do not share a right edge: {ends:?}"
	);
}

/// Push the HUD spell bar's drop targets the way the frame does — the
/// page never draws the bar, it only drops onto the rects the HUD
/// registered in the shared map.
fn with_bar_targets(r: &mut Rig) {
	let grid = hud::spell_grid(SIZE, 1.0, &hud::SafeArea::default());
	for i in 0..crate::spellbar::SPELL_SLOTS {
		r.hits.push(grid.rect(i), InvId::SpellBar(i as u8));
	}
}

/// Press at `from`, drag past the threshold to `to`, release — the drag
/// machine's Drop.
fn drag(
	r: &mut Rig,
	bar: &crate::spellbar::SpellBar,
	from: (f32, f32),
	to: (f32, f32),
) -> Option<SpellAction> {
	r.screen.handle_input(
		&InputEvent::CursorMoved {
			x: from.0,
			y: from.1,
		},
		&r.hits,
		&r.style,
		&r.view,
		bar,
	);
	r.screen.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		&r.hits,
		&r.style,
		&r.view,
		bar,
	);
	r.screen.handle_input(
		&InputEvent::CursorMoved { x: to.0, y: to.1 },
		&r.hits,
		&r.style,
		&r.view,
		bar,
	);
	r.screen.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: false,
		},
		&r.hits,
		&r.style,
		&r.view,
		bar,
	)
}

/// The drag matrix: a row assigns onto a slot (unready rows included —
/// placing is not casting), bar-to-bar moves, and off-everything clears.
#[test]
fn dragging_rows_and_slots_edits_the_bar() {
	// Broke on purpose: the wisp row is UNREADY and must still drag.
	let mut r = rig_at(digblock_core::unlock::SPHERE_AT, |_| {});
	with_bar_targets(&mut r);
	let bar = crate::spellbar::SpellBar::default();
	let grid = hud::spell_grid(SIZE, 1.0, &hud::SafeArea::default());
	let slot = |i: usize| center(grid.rect(i));
	let row = r
		.hits
		.rect_of(InvId::Spell(crate::spellbar::SLOT_WISP))
		.expect("the wisp row");

	// Row -> slot 1: assign, even though the row cannot cast right now.
	assert!(matches!(
		drag(&mut r, &bar, center(row), slot(1)),
		Some(SpellAction::AssignSlot { slot: 1, spell }) if spell == crate::spellbar::SLOT_WISP
	));
	// And the same drag as a CLICK refuses: readiness still gates casts.
	assert!(click(&mut r, center(row)).is_none());

	// Bar -> bar: a move (only an occupied slot arms, so build the state).
	let mut held = crate::spellbar::SpellBar::default();
	held.assign(1, crate::spellbar::SLOT_WISP);
	assert!(matches!(
		drag(&mut r, &held, slot(1), slot(2)),
		Some(SpellAction::MoveSlot { from: 1, to: 2 })
	));
	// Bar -> the page's own panel (off every slot): clear.
	assert!(matches!(
		drag(&mut r, &held, slot(1), (SIZE.0 as f32 / 2.0, 40.0)),
		Some(SpellAction::ClearSlot { slot: 1 })
	));
	// An EMPTY slot arms nothing: no action however far it is dragged.
	assert!(drag(&mut r, &bar, slot(0), slot(2)).is_none());
}

/// Mid-drag the page draws the spell's glyph at the cursor on Ghost —
/// the same ghost convention the inventory drag has.
#[test]
fn a_drag_shows_its_ghost() {
	let mut r = rig_at(digblock_core::unlock::SPHERE_AT, |_| {});
	with_bar_targets(&mut r);
	let bar = crate::spellbar::SpellBar::default();
	let row = r
		.hits
		.rect_of(InvId::Spell(crate::spellbar::SLOT_WISP))
		.expect("the wisp row");
	let from = center(row);
	r.screen.handle_input(
		&InputEvent::CursorMoved {
			x: from.0,
			y: from.1,
		},
		&r.hits,
		&r.style,
		&r.view,
		&bar,
	);
	r.screen.handle_input(
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
		&r.hits,
		&r.style,
		&r.view,
		&bar,
	);
	r.screen.handle_input(
		&InputEvent::CursorMoved {
			x: from.0 + 60.0,
			y: from.1 + 60.0,
		},
		&r.hits,
		&r.style,
		&r.view,
		&bar,
	);
	let dl = build(&mut r);
	let ghosts: Vec<_> = dl
		.cmds(Layer::Ghost)
		.0
		.iter()
		.filter(|c| matches!(c, Cmd::Icon { .. }))
		.collect();
	assert_eq!(ghosts.len(), 1, "one glyph rides the cursor");
}
