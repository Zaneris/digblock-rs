use super::*;
use crate::ui::draw::DrawList;
use digblock_core::block::BlockType as B;

/// A bar with nothing on it — the page names bar slots, and these rigs
/// are about the page.
fn empty_bar() -> crate::hotbar::HotbarView {
	crate::hotbar::HotbarView::build(
		&digblock_core::hotbar::Hotbar::new(),
		&Inventory::default(),
		&crate::hotbar::ToolSwap::default(),
	)
}

const SIZE: (u32, u32) = (1280, 800);

struct Rig {
	screen: AlchScreen,
	hits: HitMap<InvId>,
	view: AlchView,
	inv: Inventory,
	style: Style,
	level: u32,
}

fn skills_at(level: u32) -> SkillSet {
	let mut s = SkillSet::default();
	s.set_total(Skill::Alchemy, skills::total_xp_for(level));
	s
}

fn rig_at(level: u32, stock: impl Fn(&mut Inventory)) -> Rig {
	let mut inv = Inventory::default();
	stock(&mut inv);
	let style = Style::new(1.0);
	let mut screen = AlchScreen::default();
	screen.open();
	let view = AlchView::build(&inv, &skills_at(level));
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style,
	};
	screen.build(&mut ui, SIZE, &view, &empty_bar(), 1.0);
	Rig {
		screen,
		hits,
		view,
		inv,
		style,
		level,
	}
}

fn rebuild(r: &mut Rig) {
	r.view = AlchView::build(&r.inv, &skills_at(r.level));
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style: r.style,
	};
	r.screen.build(&mut ui, SIZE, &r.view, &empty_bar(), 1.0);
	r.hits = hits;
}

fn feed(r: &mut Rig, ev: &InputEvent) -> Option<AlchAction> {
	r.screen.handle_input(ev, &r.hits, &r.view)
}

/// Every line of text the screen draws, in emission order — for the
/// assertions that are about what a player READS.
fn text_of(r: &Rig) -> String {
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style: r.style,
	};
	r.screen.build(&mut ui, SIZE, &r.view, &empty_bar(), 1.0);
	let (cmds, arena) = dl.cmds(Layer::Content);
	cmds.iter()
		.filter_map(|c| match c {
			crate::ui::draw::Cmd::Text { range, .. } => {
				Some(&arena[range.0 as usize..range.1 as usize])
			}
			_ => None,
		})
		.collect::<Vec<_>>()
		.join(" | ")
}

/// Every text the screen draws on `layer`, with where and in what ink —
/// for the assertions about a NUMBER and its colour, which `text_of`
/// cannot make.
fn texts_on(r: &Rig, layer: Layer) -> Vec<(f32, f32, f32, u32, String)> {
	let mut dl = DrawList::default();
	let mut hits = HitMap::default();
	let mut ui = Ui {
		dl: &mut dl,
		hits: &mut hits,
		style: r.style,
	};
	r.screen.build(&mut ui, SIZE, &r.view, &empty_bar(), 1.0);
	let (cmds, arena) = dl.cmds(layer);
	cmds.iter()
		.filter_map(|c| match c {
			crate::ui::draw::Cmd::Text {
				x,
				y,
				px,
				color,
				range,
				..
			} => Some((
				*x,
				*y,
				*px,
				*color,
				arena[range.0 as usize..range.1 as usize].to_string(),
			)),
			_ => None,
		})
		.collect()
}

/// The texture layers every icon on the Icons layer samples: a flat
/// chip is one layer, a cube its faces' layers.
fn icon_layers(dl: &DrawList) -> Vec<u16> {
	dl.cmds(Layer::Icons)
		.0
		.iter()
		.filter_map(|c| match c {
			crate::ui::draw::Cmd::Icon { tex_layer, .. }
			| crate::ui::draw::Cmd::IconQuad { tex_layer, .. } => Some(*tex_layer),
			_ => None,
		})
		.collect()
}

/// The chips a rig's strip draws for the selected member, as the screen
/// itself prices them — what a test compares the draw list against.
fn strip_chips(r: &Rig, item: B) -> Vec<Chip> {
	cost_chips(
		&r.view,
		item,
		!digblock_core::unlock::can_craft(item, r.view.level),
	)
}

fn center(rect: Rect) -> (f32, f32) {
	(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
}

fn press(r: &mut Rig, at: (f32, f32)) -> Option<AlchAction> {
	feed(r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
	feed(
		r,
		&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		},
	)
}

fn family(r: &Rig, name: &str) -> InvId {
	let _ = r;
	InvId::Family(
		families()
			.iter()
			.position(|f| f.name == name)
			.unwrap_or_else(|| panic!("no {name} family")) as u8,
	)
}

/// A batch DRIPS: one paced `Craft { count: 1 }` per interval, with a
/// progress line under the button counting what is left, until the
/// queue empties or the price does.
///
/// This is the whole point of pacing a craft. The CTRL multiplier used
/// to pay a hundred crafts' XP in one frame, which made the bench's
/// one instant action also its fastest XP.
#[test]
fn a_transmute_batch_drips_instead_of_paying_at_once() {
	let mut r = rig_at(20, |inv| {
		inv.add(B::Wood, 50);
		inv.add_sub(Essence::Vitae, 500);
	});
	let slabs = center(r.hits.rect_of(family(&r, "Slabs")).unwrap());
	press(&mut r, slabs);
	rebuild(&mut r);
	feed(
		&mut r,
		&InputEvent::Key {
			key: Key::Shift,
			pressed: true,
		},
	);
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	press(&mut r, go);
	assert_eq!(
		r.screen.crafting,
		Some((B::WoodSlab, 10)),
		"SHIFT queues 10"
	);

	// Half a cycle pays nothing.
	let pace = alchemy::craft_pace_ticks(B::WoodSlab, 20) as f32 / 60.0;
	assert_eq!(r.screen.tick(pace * 0.5, &r.view), None);
	let one = Some(AlchAction::Craft {
		item: B::WoodSlab,
		count: 1,
	});
	assert_eq!(r.screen.tick(pace * 0.6, &r.view), one);
	assert_eq!(
		r.screen.crafting,
		Some((B::WoodSlab, 9)),
		"one off the queue"
	);

	// The row under the button is the batch's own progress while it
	// runs, and the modifier legend again once it stops.
	rebuild(&mut r);
	let text = text_of(&r);
	// The queue's own count reaches the row — asserted as the ARGUMENT
	// arriving, not as the sentence around it, so the row may be
	// reworded without touching this.
	let (_, left) = r.screen.crafting.expect("a batch is running");
	assert!(
		text.contains(&left.to_string()),
		"the batch never says what is left: {text}"
	);

	// The background bench: closing the screen keeps the queue, and
	// the drip keeps sending. Only leaving the world abandons.
	r.screen.close();
	assert!(r.screen.busy(), "the bench works with the screen closed");
	assert_eq!(
		r.screen.job(),
		Some(BenchJob::Craft(B::WoodSlab, 9)),
		"the HUD tell knows the job"
	);
	assert_eq!(r.screen.tick(pace * 1.1, &r.view), one);
	r.screen.open();

	// Spend the essence out from under it: the queue withdraws exactly
	// where the button would have greyed, rather than running on
	// against a rebase that can no longer pay.
	let flora = r.inv.sub_count(Essence::Vitae);
	assert!(
		r.inv.try_take_sub(Essence::Vitae, flora),
		"drain the holdings"
	);
	rebuild(&mut r);
	assert_eq!(r.screen.tick(pace * 1.1, &r.view), None);
	assert_eq!(r.screen.crafting, None, "a queue nobody can pay for ends");
	rebuild(&mut r);
	assert!(text_of(&r).contains("CLICK 1"), "the legend is back");

	// The world-exit arm clears outright.
	r.screen.debug_batch(B::WoodSlab, 3, 0.5);
	assert!(r.screen.busy());
	r.screen.abandon();
	assert!(!r.screen.busy(), "abandon drops the job");
}

/// The transmute flow: pick a family tile, pick a member, and the
/// page's ONE button queues it — multipliers clamp to affordability,
/// and a locked selection disables the button outright (no hit).
#[test]
fn family_selection_and_the_single_transmute_button() {
	// A level past the mason's unlock and short of dark lumber's, so
	// the rig holds an unlocked-but-unaffordable member (Stone, with
	// no Earth in hand) AND a locked one (DarkLumber) at once.
	let mut r = rig_at(15, |inv| {
		inv.add(B::Wood, 50);
		inv.add_sub(Essence::Vitae, 500);
	});

	// It opens on the family row alone: no member is pre-picked, so
	// there is nothing for the one button to act on and it isn't
	// drawn.
	assert!(
		r.hits.rect_of(InvId::Craft(0)).is_none(),
		"no members before a family is picked"
	);
	assert!(
		r.hits.rect_of(InvId::CraftGo).is_none(),
		"no button before a family is picked"
	);

	// SLABS leads (the shape axis first, cheapest form first, the
	// hand-priced ones after). Its first unlocked member is the
	// WoodSlab the Vitae affords; Ctrl asks for its whole batch and
	// the holdings clamp it.
	let slabs = center(r.hits.rect_of(family(&r, "Slabs")).unwrap());
	press(&mut r, slabs);
	rebuild(&mut r);
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	feed(
		&mut r,
		&InputEvent::Key {
			key: Key::Ctrl,
			pressed: true,
		},
	);
	let batches = r.view.affordable(B::WoodSlab);
	assert!(batches > 0 && batches < BULK_CTRL, "clamp is meaningful");
	// The press QUEUES the batch rather than paying for it.
	assert_eq!(press(&mut r, go), None, "the button starts a batch");
	assert_eq!(r.screen.crafting, Some((B::WoodSlab, batches)));
	// Pressing it again is the stop, so the bench is idle for the
	// selection checks below.
	rebuild(&mut r);
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	assert_eq!(press(&mut r, go), None);
	assert_eq!(r.screen.crafting, None, "the second press cancels");
	feed(
		&mut r,
		&InputEvent::Key {
			key: Key::Ctrl,
			pressed: false,
		},
	);

	// BLOCKS: switching family lands on the first unlocked member —
	// Lumber, since the grid puts a material's whole learned set at
	// the material's own level and planed timber is the first
	// substance the bench makes in cube form. (The felled log is not
	// here: the world hands you that one, so it has no recipe.) The
	// one button queues THAT.
	let blocks = center(r.hits.rect_of(family(&r, "Blocks")).unwrap());
	press(&mut r, blocks);
	rebuild(&mut r);
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	assert_eq!(press(&mut r, go), None);
	assert_eq!(r.screen.crafting, Some((B::Lumber, 1)));
	rebuild(&mut r);
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	press(&mut r, go);
	assert_eq!(r.screen.crafting, None, "back to idle");
	// A member locked at this rig's level: the tile still shows (the
	// strip names the level it wants), but the single button registers
	// no hit — honest pixels, not a trap.
	let dark = center(r.hits.rect_of(InvId::Craft(2)).unwrap());
	press(&mut r, dark);
	rebuild(&mut r);
	assert!(
		r.hits.rect_of(InvId::Craft(2)).is_some(),
		"locked member still shows"
	);
	assert!(
		r.hits.rect_of(InvId::CraftGo).is_none(),
		"no button under a locked pick"
	);
	// And picking an unlocked-but-unaffordable member by tile keeps the
	// button away: "can't yet" and "can't afford" are different
	// answers, and neither leaves a hit rect.
	let stone = center(r.hits.rect_of(InvId::Craft(1)).unwrap());
	press(&mut r, stone);
	rebuild(&mut r);
	assert!(r.hits.rect_of(InvId::CraftGo).is_none(), "Stone: no Earth");
}

/// The vessels are paid in FEEDSTOCK: once both are open, a player with
/// timber can make the bucket and the siphon with no essence anywhere —
/// the whole reason the economy can bootstrap — and the strip names the
/// missing timber when they cannot. Rigged at the later vessel's own
/// rung, read off `unlock`, so a move of either keeps the page honest.
#[test]
fn the_vessels_craft_from_timber_alone() {
	let rung = digblock_core::unlock::craft(B::Bucket)
		.max(digblock_core::unlock::craft(B::EssenceSiphon))
		.expect("both vessels are crafts");
	let mut r = rig_at(rung, |inv| inv.add(B::Wood, 20));
	let tools = center(r.hits.rect_of(family(&r, "Tools")).unwrap());
	press(&mut r, tools);
	rebuild(&mut r);
	// Lands on the bucket — the first unlocked member.
	assert!(
		r.view.affordable(B::Bucket) > 0,
		"timber affords the bucket"
	);
	assert!(
		r.view.affordable(B::EssenceSiphon) > 0,
		"and the essence tool"
	);
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	press(&mut r, go);
	assert_eq!(r.screen.crafting, Some((B::Bucket, 1)));
	// Cancel (the second press), keeping the family selection.
	rebuild(&mut r);
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	press(&mut r, go);
	assert_eq!(r.screen.crafting, None);

	// Short on timber: the strip names the FEEDSTOCK shortfall, not an
	// essence that cannot help.
	let wood = r.inv.count(B::Wood);
	assert!(r.inv.try_take(B::Wood, wood), "drain the timber");
	rebuild(&mut r);
	assert_eq!(
		strip_reason(&r.view, B::Bucket),
		StripReason::Short {
			of: ShortOf::Feedstock(B::Wood),
			more: alchemy::craft_feedstock(B::Bucket)
				.iter()
				.find(|(f, _)| *f == B::Wood)
				.map(|&(_, n)| n)
				.expect("the bucket is priced in timber")
		},
		"the strip must name the missing TIMBER, not an essence that cannot help"
	);
	// And the shortfall reaches the screen as a CHIP, never a word: the
	// timber line's icon is the Wood cube, its number is the price, and
	// its ink is the short one — while no text anywhere names the block.
	let chips = strip_chips(&r, B::Bucket);
	let wood = chips
		.iter()
		.find(|c| crate::ui::hud::icon_block(c.kind) == Some(B::Wood))
		.expect("the bucket's price has a timber chip");
	assert_eq!(wood.ink, theme::SHORT_INK, "the short line reads short");
	let texts = texts_on(&r, Layer::Content);
	assert!(
		texts
			.iter()
			.any(|(_, _, _, ink, t)| *t == wood.n.to_string() && *ink == theme::SHORT_INK),
		"the timber count is drawn in the short ink"
	);
	assert!(
		!texts
			.iter()
			.any(|(_, _, _, _, t)| t.contains(&B::Wood.name().to_uppercase())),
		"a price is chips, not words"
	);
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
		&empty_bar(),
		1.0,
	);
	assert!(
		icon_layers(&dl).contains(&B::Wood.props().tex_top),
		"the timber chip draws the Wood cube"
	);
	assert!(
		r.hits.rect_of(InvId::CraftGo).is_none(),
		"no button with nothing to pay"
	);
}

/// The bug this page's chips exist to fix: with a name and a price
/// sharing one line, the price was dropped whenever the line did not
/// fit, and half the catalog's did not. Now every visible member shows
/// every line of its price, at every rung the fit ladder reaches.
#[test]
fn the_strip_never_drops_a_price() {
	let mut examined = 0;
	for scale in [1.0f32, 1.75, 2.5] {
		let mut r = rig_at(99, |inv| {
			for b in B::ALL {
				inv.add(b, 999);
			}
			for e in Essence::ALL {
				inv.add_sub(e, 99_999);
			}
		});
		r.style = Style::new(scale);
		for (fi, f) in families().iter().enumerate() {
			for (mi, &item) in f.members.iter().enumerate() {
				r.screen.family = Some(fi);
				r.screen.member = mi;
				rebuild(&mut r);
				let want = alchemy::craft_feedstock(item).len()
					+ alchemy::craft_ideal(item)
						.expect("a member is craftable")
						.len();
				let chips = strip_chips(&r, item);
				assert_eq!(chips.len(), want, "{item:?} prices {want} lines");
				// Each line's number is drawn, in the row's ink, with its
				// icon beside it: count the chip numbers the strip shows.
				let texts = texts_on(&r, Layer::Content);
				for c in &chips {
					assert!(
						texts
							.iter()
							.any(|(_, _, _, ink, t)| *t == c.n.to_string() && *ink == c.ink),
						"{item:?} at {scale}: a {} line is not drawn",
						chip_name(c)
					);
				}
				examined += 1;
			}
		}
	}
	assert!(examined > 20, "the catalog has shrunk");
}

/// The panel bottom-anchors above the hotbar, and a short window at a
/// big scale fits by stepping the ladder.
#[test]
fn panel_sits_above_the_hotbar_and_fits_short_windows() {
	for (size, scale) in [
		((1280u32, 800u32), 1.0f32),
		((1280, 720), 1.75),
		((960, 600), 2.5),
	] {
		let inv = Inventory::default();
		let view = AlchView::build(&inv, &skills_at(41));
		let mut screen = AlchScreen::default();
		screen.open();
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: Style::new(scale),
		};
		screen.build(&mut ui, size, &view, &empty_bar(), 1.0);
		let bar_top = hud::row_grid(size, scale).origin.1;
		let panel = dl
			.cmds(Layer::Backdrop)
			.0
			.iter()
			.find_map(|c| match c {
				crate::ui::draw::Cmd::Quad { rect, color } if *color == theme::PANEL_BG => {
					Some(*rect)
				}
				_ => None,
			})
			.expect("panel quad");
		assert!(panel.y >= 0.0, "panel top on screen at {size:?}/{scale}");
		assert!(
			panel.y + panel.h <= bar_top + 0.5,
			"panel under the bar at {size:?}/{scale}: {} vs {bar_top}",
			panel.y + panel.h
		);
	}
}

/// The essence readout stays INSIDE the panel, at every rung of the fit
/// ladder and with the holdings at their widest.
///
/// It is the one row on the page whose width is DATA, and it does not
/// shrink with the panel: `glyph_px` floors at a whole pixel, so the
/// ladder's bottom rungs take a quarter off every box while the text stays
/// where it is. Sizing the page on the inventory grid's extent alone spilled
/// this row past the panel edge onto the dimmed world, unclipped and growing
/// with the digits.
///
/// Swept at the scales the fit ladder actually reaches, with a full stack of
/// every essence so the string is as long as the game can make it.
#[test]
fn the_essence_readout_never_spills_out_of_its_panel() {
	let mut inv = Inventory::default();
	for &e in &Essence::ALL {
		inv.add_sub(e, u32::MAX);
	}
	let view = AlchView::build(&inv, &skills_at(41));
	for (size, scale) in [
		((1280u32, 800u32), 1.0f32),
		((844, 390), 2.25),
		((844, 390), 3.0),
		((667, 375), 3.0),
		((932, 430), 3.0),
		// Portrait, where `row_w`'s clamp to the window bites and
		// widening the panel cannot be the whole answer.
		((390, 844), 3.0),
	] {
		let mut screen = AlchScreen::default();
		screen.open();
		let mut dl = DrawList::default();
		let mut hits = HitMap::default();
		let mut ui = Ui {
			dl: &mut dl,
			hits: &mut hits,
			style: Style::new(scale),
		};
		screen.build(&mut ui, size, &view, &empty_bar(), 1.0);
		let panel = dl
			.cmds(Layer::Backdrop)
			.0
			.iter()
			.find_map(|c| match c {
				crate::ui::draw::Cmd::Quad { rect, color } if *color == theme::PANEL_BG => {
					Some(*rect)
				}
				_ => None,
			})
			.expect("panel quad");
		// The readout is the run of essence chips: each icon, and the
		// number sharing its row top. Measured at the command's OWN face
		// and px — the page draws at the fitted scale, not the window
		// scale this loop set.
		let icons: Vec<Rect> = dl
			.cmds(Layer::Icons)
			.0
			.iter()
			.filter_map(|c| match c {
				crate::ui::draw::Cmd::Icon {
					rect, tex_layer, ..
				} if Essence::ALL.iter().any(|e| e.icon_tex() == *tex_layer) => Some(*rect),
				_ => None,
			})
			.collect();
		assert_eq!(icons.len(), Essence::COUNT, "every holding is drawn");
		let (cmds, arena) = dl.cmds(Layer::Content);
		let numbers = cmds.iter().filter_map(|c| match c {
			crate::ui::draw::Cmd::Text {
				face,
				x,
				y,
				px,
				range,
				..
			} if icons.iter().any(|r| (r.y - y).abs() < 0.5) => {
				let t = &arena[range.0 as usize..range.1 as usize];
				Some(x + crate::font::face(*face).measure(t, *px))
			}
			_ => None,
		});
		let right = icons
			.iter()
			.map(|r| r.x + r.w)
			.chain(numbers)
			.fold(0.0, f32::max);
		assert!(
			right <= panel.x + panel.w + 0.5,
			"readout spills past the panel at {size:?}/{scale}: {right} vs {}",
			panel.x + panel.w
		);
	}
}

/// The family tile rotates rather than cutting: a member holds at
/// full presence, shrinks away, and the NEXT one grows in — with the
/// handover happening at zero, so two icons never overlap.
#[test]
fn family_tiles_transition_instead_of_cutting() {
	let hold = FAMILY_CYCLE_SECS - FAMILY_SWAP_SECS;
	// The hold: member 0, fully present, start to just before the swap.
	for t in [0.0, hold * 0.5, hold - 0.001] {
		assert_eq!(family_preview(t, 3), (0, 1.0), "t={t}");
	}
	// The swap: presence falls to zero on the outgoing member...
	let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.25, 3);
	assert_eq!(i, 0);
	assert!((p - 0.5).abs() < 1e-4, "half faded: {p}");
	let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.5 - 0.001, 3);
	assert_eq!(i, 0);
	assert!(p < 0.01, "gone at the handover: {p}");
	// ...and the next one grows from zero.
	let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.5 + 0.001, 3);
	assert_eq!(i, 1, "handover at the bottom of the dip");
	assert!(p < 0.01, "arrives from nothing: {p}");
	let (i, p) = family_preview(hold + FAMILY_SWAP_SECS * 0.999, 3);
	assert_eq!(i, 1);
	assert!(p > 0.99, "back to full by the next hold: {p}");
	// Presence is never negative and never over 1, anywhere.
	for n in 1..=8 {
		let mut t = 0.0f32;
		while t < 20.0 {
			let (i, p) = family_preview(t, n);
			assert!(i < n, "index in range at {t} (n={n})");
			assert!((0.0..=1.0).contains(&p), "presence {p} at {t}");
			t += 0.011;
		}
	}
	// One member: nothing to trade for, so no pulsing at all.
	for t in [0.0, 0.5, hold + 0.2, 5.0] {
		assert_eq!(family_preview(t, 1), (0, 1.0), "t={t}");
	}
	// And it really does cycle: over one full lap of a 2-member
	// family both members hold.
	let seen: std::collections::HashSet<usize> = (0..200)
		.map(|k| family_preview(k as f32 * 0.02, 2).0)
		.collect();
	assert_eq!(seen.len(), 2);
}

/// The families are derived from the shape axis, so a new material ×
/// shape pair reaches the screen on its own — but a hand-priced
/// FUNCTIONAL item still has to be filed by hand, and this is what
/// catches one that wasn't. Every craftable appears in exactly one
/// family; nothing else appears at all.
#[test]
fn every_craftable_appears_in_exactly_one_family() {
	for &b in &BlockType::ALL {
		let craftable = unlock::craft(b).is_some();
		let appearances: usize = families()
			.iter()
			.map(|f| f.members.iter().filter(|&&m| m == b).count())
			.sum();
		assert_eq!(
			appearances,
			usize::from(craftable),
			"{b:?}: craftable blocks sit in exactly one family, others in none"
		);
	}
}

/// The page shows what is unlocked plus ONE teaser tier — the next
/// unlock level, all of whose items show together — and hides the
/// rest, families included. Hiding must not shift identity: tiles
/// pack left but keep their true indices, so a member's hit id is the
/// same at every level it is visible at.
#[test]
fn the_page_hides_everything_past_the_next_unlock() {
	// A fresh player: the slab and the two vessels are unlocked, the
	// stairs shape is the next tier — and that is the WHOLE page.
	let mut r = rig_at(1, |inv| inv.add(B::Wood, 20));
	let idx = |name: &str| {
		families()
			.iter()
			.position(|f| f.name == name)
			.unwrap_or_else(|| panic!("no {name} family")) as u8
	};
	for name in ["Slabs", "Stairs", "Tools"] {
		assert!(
			r.hits.rect_of(InvId::Family(idx(name))).is_some(),
			"{name} belongs on a fresh page"
		);
	}
	for name in ["Doors", "Blocks", "Lights", "Furniture"] {
		assert!(
			r.hits.rect_of(InvId::Family(idx(name))).is_none(),
			"{name} is past the next unlock and must hide"
		);
	}
	// Inside TOOLS: the vessels show, the pick and the sigil do not —
	// the sigil at the cap is exactly the tile a fresh player must
	// not be stared down by.
	let tools = center(r.hits.rect_of(InvId::Family(idx("Tools"))).unwrap());
	press(&mut r, tools);
	rebuild(&mut r);
	assert!(r.hits.rect_of(InvId::Craft(0)).is_some(), "the bucket");
	assert!(r.hits.rect_of(InvId::Craft(1)).is_some(), "the siphon");
	assert!(r.hits.rect_of(InvId::Craft(2)).is_none(), "the pick hides");
	assert!(r.hits.rect_of(InvId::Craft(3)).is_none(), "the sigil hides");

	// Two things sharing the next level BOTH show: at the mason's
	// unlock the next tier holds dark lumber and stone brick, and the
	// true member indices survive the packing.
	let mut r = rig_at(15, |_| {});
	let blocks = center(r.hits.rect_of(InvId::Family(idx("Blocks"))).unwrap());
	press(&mut r, blocks);
	rebuild(&mut r);
	let members = &families()[idx("Blocks") as usize].members;
	let at = |b: B| members.iter().position(|&m| m == b).unwrap() as u8;
	assert!(r.hits.rect_of(InvId::Craft(at(B::Lumber))).is_some());
	assert!(r.hits.rect_of(InvId::Craft(at(B::Stone))).is_some());
	assert!(
		r.hits.rect_of(InvId::Craft(at(B::DarkLumber))).is_some(),
		"the next tier shows"
	);
	assert!(
		r.hits.rect_of(InvId::Craft(at(B::StoneBrick))).is_some(),
		"BOTH of the next tier's items show"
	);
	assert!(
		r.hits.rect_of(InvId::Craft(at(B::Sandstone))).is_none(),
		"one tier past the next stays hidden"
	);
	// And at the cap nothing hides at all.
	let r = rig_at(99, |_| {});
	for name in [
		"Slabs",
		"Stairs",
		"Doors",
		"Blocks",
		"Lights",
		"Furniture",
		"Tools",
	] {
		assert!(
			r.hits.rect_of(InvId::Family(idx(name))).is_some(),
			"{name} at the cap"
		);
	}
}

/// Owning a tool is a yes/no (§7.3), so the bench sells exactly one
/// and then stops offering it. `apply_alchemy` refuses a craft that
/// would vanish into the cap — an offer the rule refuses is one the
/// player pays a tool's whole price for and gets nothing back — and
/// the strip says WHY: a cap is not a shortfall, and naming a cost
/// here would send them off to gather something that cannot help.
#[test]
fn an_owned_tool_is_no_longer_on_offer() {
	let mut r = rig_at(99, |inv| {
		for &e in &Essence::ALL {
			inv.add_sub(e, 5_000);
		}
	});
	let tools = center(r.hits.rect_of(family(&r, "Tools")).unwrap());
	press(&mut r, tools);
	rebuild(&mut r);
	let member = families()
		.iter()
		.find(|f| f.name == "Tools")
		.unwrap()
		.members
		.iter()
		.position(|&m| m == B::ResonantPick)
		.expect("the pick is a tool");
	let at = center(r.hits.rect_of(InvId::Craft(member as u8)).unwrap());
	press(&mut r, at);
	rebuild(&mut r);

	assert_eq!(r.view.affordable(B::ResonantPick), 1, "one, never a batch");
	let go = center(r.hits.rect_of(InvId::CraftGo).unwrap());
	assert_eq!(press(&mut r, go), None, "the press queues the forge");
	// And the forge takes the longest hold in the catalog — the
	// biggest price is the biggest piece of work.
	let pace = alchemy::craft_pace_ticks(B::ResonantPick, 99) as f32 / 60.0;
	assert!(
		pace > alchemy::craft_pace_ticks(B::LumberSlab, 99) as f32 / 60.0,
		"a pick should not forge as fast as a slab"
	);
	assert_eq!(r.screen.tick(pace * 0.9, &r.view), None, "mid-hold");
	assert_eq!(
		r.screen.tick(pace * 0.2, &r.view),
		Some(AlchAction::Craft {
			item: B::ResonantPick,
			count: 1
		})
	);
	assert_eq!(r.screen.crafting, None, "a batch of one ends with it");

	// The server's answer arrives: it's in the bag. The offer closes.
	r.inv.add(B::ResonantPick, 1);
	rebuild(&mut r);
	assert_eq!(r.view.affordable(B::ResonantPick), 0);
	assert!(
		r.hits.rect_of(InvId::CraftGo).is_none(),
		"no button on an item that cannot be crafted"
	);
	// A full cap is not a shortfall: naming a price here would send the
	// player off to gather something that cannot help.
	assert_eq!(
		strip_reason(&r.view, B::ResonantPick),
		StripReason::AtCap { tool: true }
	);
}

/// Every text on the Ghost layer — the tooltip's, and nothing else's
/// while no drag is running.
fn ghost_texts(dl: &DrawList) -> Vec<String> {
	let (cmds, arena) = dl.cmds(Layer::Ghost);
	cmds.iter()
		.filter_map(|c| match c {
			crate::ui::draw::Cmd::Text { range, .. } => {
				Some(arena[range.0 as usize..range.1 as usize].to_string())
			}
			_ => None,
		})
		.collect()
}

/// Every tile and chip on the page names itself on hover, and the name
/// is DERIVED — the family's, the member's, the essence's — with
/// nothing drawn when the cursor is on none of them.
#[test]
fn every_tile_and_chip_names_itself_on_hover() {
	let mut r = rig_at(99, |inv| {
		for b in B::ALL {
			inv.add(b, 99);
		}
		for e in Essence::ALL {
			inv.add_sub(e, 999);
		}
	});
	let frame = |r: &Rig| {
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
			&empty_bar(),
			1.0,
		);
		(ghost_texts(&dl), dl)
	};
	assert!(frame(&r).0.is_empty(), "nothing hovered, nothing named");
	let mut named = 0;
	// Every family tile, then every member of the last one picked.
	for (i, fam) in families().iter().enumerate() {
		let Some(rect) = r.hits.rect_of(InvId::Family(i as u8)) else {
			continue;
		};
		let at = center(rect);
		feed(&mut r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
		assert_eq!(frame(&r).0, vec![fam.name.to_uppercase()], "family {i}");
		named += 1;
		press(&mut r, at);
		rebuild(&mut r);
		for (j, &item) in fam.members.iter().enumerate() {
			let Some(rect) = r.hits.rect_of(InvId::Craft(j as u8)) else {
				continue;
			};
			let at = center(rect);
			feed(&mut r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
			assert_eq!(frame(&r).0, vec![item.name().to_uppercase()], "{item:?}");
			named += 1;
		}
	}
	// The readout's chips: each essence icon names its essence.
	feed(&mut r, &InputEvent::CursorMoved { x: -1.0, y: -1.0 });
	let (_, dl) = frame(&r);
	let chips: Vec<(Rect, Essence)> = dl
		.cmds(Layer::Icons)
		.0
		.iter()
		.filter_map(|c| match c {
			crate::ui::draw::Cmd::Icon {
				rect, tex_layer, ..
			} => Essence::ALL
				.iter()
				.find(|e| e.icon_tex() == *tex_layer)
				.map(|e| (*rect, *e)),
			_ => None,
		})
		.collect();
	assert!(chips.len() >= Essence::COUNT, "the readout is drawn");
	for (rect, e) in chips {
		let at = center(rect);
		feed(&mut r, &InputEvent::CursorMoved { x: at.0, y: at.1 });
		assert_eq!(frame(&r).0, vec![e.name().to_uppercase()], "{e:?} chip");
		named += 1;
	}
	assert!(
		named > families().len() + Essence::COUNT,
		"too little examined"
	);
}

/// The legend under the button names the multipliers the keys really
/// apply — a literal here went stale once.
#[test]
fn the_legend_names_the_real_multipliers() {
	let mut r = rig_at(1, |_| {});
	// The legend describes the button, and waits with it for a pick.
	let tools = center(r.hits.rect_of(family(&r, "Tools")).unwrap());
	press(&mut r, tools);
	rebuild(&mut r);
	let text = text_of(&r);
	for want in [format!("SHIFT {BULK_SHIFT}"), format!("CTRL {BULK_CTRL}")] {
		assert!(text.contains(&want), "{want} missing from {text}");
	}
}
