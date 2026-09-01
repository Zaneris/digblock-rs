//! What a click does to the world — targeting, digging, placing, the
//! pick's plane and doors. Every rule here is mirrored server-side;
//! the client half is prediction and may be rejected.

use super::*;

impl Engine {
	/// True while the water tool is the selected hotbar item.
	/// `tool_in_hand` asks both halves at once — the slot is selected AND
	/// the tool in it is the bucket — which is the same predicate every
	/// other tool gate now reads.
	pub(super) fn water_tool_selected(&self) -> bool {
		self.hotbar.tool_in_hand() == BlockType::Bucket
	}

	/// The owned tool roster in deterministic block-id order: the always-owned bucket plus every
	/// tool id with a nonzero count. The predicate lives in core so the rail this feeds and the
	/// ownership check `reconcile_layout` makes are literally the same question — a client offering
	/// a tool its own server would take back is a rejected swing, not a feature.
	pub(super) fn owned_tools(&self) -> Vec<BlockType> {
		self.inventory.owned_tools().collect()
	}

	/// True while the tool is out AND empty — the scooping state, and the one that changes the
	/// picking ray, because it is the only state in which the thing you want to hit is a fluid
	/// source. Charged, the bucket picks with the ordinary ray, so the outline lands on the face
	/// you are about to pour against.
	pub(super) fn scoop_ready(&self) -> bool {
		self.water_tool_selected() && !self.inventory.bucket_full()
	}

	/// Where the collidable body actually is, for occupancy questions ("would this placement trap
	/// me?"). Deliberately NOT `camera.pos`: the camera carries the tick interpolation and the
	/// decaying reconciliation offset, so it can sit up to `POS_TOLERANCE` from the body, and the
	/// server answers the same question from its own simulated position. Aim rays still come from
	/// the camera — you hit what you see.
	pub(super) fn body_pos(&self) -> Vec3 {
		match self.mode {
			CameraMode::FirstPerson | CameraMode::Cinematic(_) => self.player.pos,
			CameraMode::DebugFly => self.camera.pos, // fly leaves player.pos stale
		}
	}

	/// The player's PRESENTED eye: what first person is (or would be)
	/// showing. Streaming, the carried light, and the body-sense probes
	/// anchor here, so a detached camera cannot drag them along. Today
	/// every mode answers with the camera; a cinematic rig answers with
	/// the first-person eye it detached from.
	pub(super) fn body_anchor(&self) -> Vec3 {
		match self.mode {
			CameraMode::FirstPerson | CameraMode::DebugFly => self.camera.pos,
			CameraMode::Cinematic(_) => self.fp_eye,
		}
	}

	/// Adopt a facing outright — both the player's angles and the camera
	/// showing them, so a restore or an authoritative turn lands this
	/// frame rather than after the next input copy.
	pub(super) fn set_look(&mut self, yaw_deg: f32, pitch_deg: f32) {
		self.look.yaw_deg = yaw_deg;
		self.look.pitch_deg = pitch_deg.clamp(-89.0, 89.0);
		self.camera.yaw_deg = self.look.yaw_deg;
		self.camera.pitch_deg = self.look.pitch_deg;
	}

	/// May a placement write this cell? Resident, and holding a block the
	/// shared core predicate calls replaceable. The server asks exactly
	/// this (`Sim::cell_is_replaceable`), so a place predicted here is
	/// never one the server turns around and rejects.
	pub(super) fn cell_is_replaceable(&self, cell: (i32, i32, i32)) -> bool {
		self.pipeline
			.cell_at(cell.0, cell.1, cell.2)
			.is_some_and(|c| BlockType::from_id(digblock_core::chunk::cell_id(c)).is_replaceable())
	}

	/// Recompute the crosshair target (every update — it drives the
	/// highlight overlay, the dig state machine, and placement).
	pub(super) fn update_target(&mut self) {
		// Detached: there is no crosshair over a cinematic, and everything
		// downstream (highlight, dig, place) is None-tolerant.
		if matches!(self.mode, CameraMode::Cinematic(_)) {
			self.target = None;
			return;
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		let pipeline = &self.pipeline;
		let cell_at = |x: i32, y: i32, z: i32| pipeline.cell_at(x, y, z);
		self.target = if self.scoop_ready() {
			raycast_fluid(origin, dir, REACH, &cell_at)
		} else {
			raycast(origin, dir, REACH, &cell_at)
		};
	}

	/// The offline spawn as resolved against the save's edits — the eye the arrival lock centres
	/// on, through the same edit-aware resolve the server runs. Split from [`Self::local_spawn`]
	/// because the lock must anchor on the PAD, before the landing rule steps over any residual
	/// obstruction. A world with no overlay has no recorded edits to reason about, so the anchor
	/// stands and `ascend_clear` still steps over anything built there this session.
	pub(super) fn local_spawn_resolved(&self) -> Vec3 {
		match self.pipeline.overlay() {
			Some(o) => digblock_core::spawn::resolve(self.save.spawn_pos, self.seed, &|x, y, z| {
				o.column(x.div_euclid(16), z.div_euclid(16))
					.and_then(|cells| cells.get(&(x, y, z)).copied())
			}),
			None => self.save.spawn_pos,
		}
	}

	pub(super) fn local_spawn(&self) -> Vec3 {
		let resolved = self.local_spawn_resolved();
		let pipeline = &self.pipeline;
		digblock_core::player::ascend_clear(resolved, &|x, y, z| pipeline.cell_at(x, y, z))
	}

	/// What a dig on this cell IS for us right now (SKILLS-DESIGN §5.3) — the same `dig_mode` the
	/// server asks of its own world, so prediction and authority cannot disagree about whether an
	/// action was a dig, a draw, or a refusal. The chunk store still holds Stone here: the reveal
	/// is a stamp on the MESH input, never a world write, which keeps ore out of the journal and
	/// out of everyone else's world.
	pub(super) fn node_dig_mode(
		&self,
		cell: (i32, i32, i32),
	) -> digblock_core::worldgen::nodes::DigMode {
		use digblock_core::worldgen::nodes::DigMode;
		let Some(v) = self.pipeline.cell_at(cell.0, cell.1, cell.2) else {
			return DigMode::Break;
		};
		digblock_core::worldgen::nodes::dig_mode(
			v,
			cell,
			self.seed,
			digblock_core::worldgen::nodes::Levels::of(&self.skills, &self.inventory),
			self.pipeline.cell_edited(cell.0, cell.1, cell.2),
			self.hotbar.tool_in_hand(),
		)
	}

	/// Has this cell ever been written — the provenance half of `BlockType::dig_credit`, and the
	/// same bit `node_dig_mode` above passes to the reveal. Read off the `EditOverlay`, which
	/// MIRRORS the server's journal in multiplayer, so both ends answer alike. A place of one's own
	/// is recorded when the server echoes it, not when it is predicted — recording predictions is
	/// unfixable, since a rejection reverts through `set_cell` and `record` is insert-only.
	pub(super) fn cell_edited(&self, cell: (i32, i32, i32)) -> bool {
		self.pipeline.cell_edited(cell.0, cell.1, cell.2)
	}

	/// The EXTRA cells a Resonant Pick hold breaks around `hit` — empty
	/// unless the pick is in hand at its own Mining level. The set is
	/// derived from the ray, never chosen: the server re-raycasts and
	/// runs the same `pick::cells` over its own world.
	pub(super) fn pick_plane(&self, hit: &RayHit) -> Vec<(i32, i32, i32)> {
		// `tool_in_hand`, not `active_tool`: the plane belongs to the pick
		// being SWUNG. The hold multiplier reads the same answer, so a
		// block dug with dirt in hand can never be priced as a pick swing
		// and broken as a bare one.
		if self.hotbar.tool_in_hand() != BlockType::ResonantPick {
			return Vec::new();
		}
		let level = self.skills.level(digblock_core::skills::Skill::Mining);
		let alchemy = self.skills.level(digblock_core::skills::Skill::Alchemy);
		let width = digblock_core::unlock::pick_cells(level);
		let id = digblock_core::chunk::cell_id(hit.cell_value);
		let credit = hit.block.dig_credit(self.cell_edited(hit.cell), alchemy);
		let mut out = digblock_core::pick::cells(
			hit.cell,
			hit.normal,
			self.camera.dir().to_array(),
			width,
			// The same clamp the server runs over the same width - the two
			// footprints stay bit-identical.
			self.dig_orientation.clamped(width),
			// The same rule the server runs over its own world — including
			// the world floor, which `dig_hardness` refuses at exactly the
			// y `protect::immutable` does. A server's spawn lock is its own
			// secret and stays unpredicted, as everywhere else.
			|c| {
				self.pipeline.cell_at(c.0, c.1, c.2).is_some_and(|v| {
					digblock_core::pick::breaks_as_extra(
						id,
						credit,
						v,
						c,
						self.node_dig_mode(c),
						self.cell_edited(c),
						alchemy,
					)
				})
			},
		);
		// The aimed cell leads the list and the caller already has it.
		out.retain(|&c| c != hit.cell);
		out
	}

	pub(super) fn diggable(&self, hit: &RayHit) -> bool {
		// Permanently protected cells never start a hold: the world floor
		// everywhere, plus the offline arrival regions. A SERVER's regions
		// are its own secret, so a cell it has already refused is
		// remembered instead.
		if digblock_core::protect::immutable(hit.cell.0, hit.cell.1, hit.cell.2, &self.protected)
			|| self.refused_cell == Some(hit.cell)
		{
			return false;
		}
		// The skill gates are fully client-predictable (we know our own
		// levels); the server's DigStart refusals are the backstop, and it
		// asks this same question.
		if digblock_core::unlock::dig_blocked(hit.block, &self.skills).is_some() {
			return false;
		}
		// A revealed vein and the wrong tool: predicted here so the hold
		// never opens, exactly as the server refuses it at `DigStart`.
		if self.node_dig_mode(hit.cell) == digblock_core::worldgen::nodes::DigMode::NeedsSiphon {
			return false;
		}
		if self.water_tool_selected() {
			// A charged bucket digs nothing at all: it has one action
			// left, and that action is on the other button.
			self.scoop_ready() && hit.block.is_fluid_source()
		} else {
			hit.block.props().solid && hit.block.dig_hardness(hit.cell.1).is_some()
		}
	}

	/// Drop every live touch: the driver's look/move/dig state, the HUD's
	/// claims, and the held state they were driving. The one call every
	/// "touches are gone" site makes (focus loss, a screen opening), so a
	/// held button can never survive its own finger.
	pub(super) fn clear_touch_state(&mut self) {
		self.input.touch.clear();
		self.input.hud_touches.clear();
		self.input.hud_jump_held = false;
	}

	/// Physical-px display cutouts (notch, home indicator) the touch HUD
	/// keeps its buttons clear of. Shell-forwarded; zero when the shell
	/// cannot measure any.
	pub fn set_safe_area(&mut self, insets: crate::ui::hud::SafeArea) {
		self.input.safe_area = insets;
	}

	/// Begin a cast channel. A TELEPORT on cooldown opens the spellbook
	/// instead — the countdown is visible there, and a silent no-op
	/// reads as a broken key. One clock covers Return Home and every
	/// port, so neither can start while the other's window runs.
	/// Apply what a spellbook click (or a slot activation) asked for. The
	/// cast arms close the book — the cast is watched, not menued; a
	/// closed book closes harmlessly on the hotkey path.
	pub(super) fn apply_spell_action(&mut self, act: crate::ui::screens::spellbook::SpellAction) {
		use crate::ui::screens::spellbook::SpellAction as A;
		match act {
			A::CastHome => {
				self.ui.spellbook.close();
				self.start_cast(CastTarget::Home);
			}
			A::CastWaystone(idx) => {
				self.ui.spellbook.close();
				self.start_cast(CastTarget::Waystone(idx));
			}
			A::CastWisp => {
				self.ui.spellbook.close();
				self.start_cast(CastTarget::Wisp);
			}
			A::CastKindle => {
				self.ui.spellbook.close();
				self.start_cast(CastTarget::Kindle);
			}
			A::CastSphere => {
				self.ui.spellbook.close();
				self.start_cast(CastTarget::Sphere);
			}
			A::ToggleCauseway => {
				self.ui.spellbook.close();
				// Taking a road up is not a cast and channels like
				// nothing: the caster wanting to descend wants it gone now.
				if self.causeway_left > 0.0 {
					self.end_causeway();
					if let Some(net) = self.session.client.as_mut() {
						net.send_causeway_end();
					}
				} else {
					self.start_cast(CastTarget::Causeway);
				}
			}
			// The bar edits keep the book OPEN — binding two slots in a
			// row is the normal gesture. Persistence rides the same
			// change detection the item layout uses (`update`'s sync).
			A::AssignSlot { slot, spell } => self.spell_bar.assign(slot as usize, spell),
			A::MoveSlot { from, to } => self.spell_bar.swap(from as usize, to as usize),
			A::ClearSlot { slot } => self.spell_bar.clear(slot as usize),
			A::ActivateSlot(k) => self.activate_spell_slot(k as usize),
		}
	}

	/// Whether the orientation indicator is on screen: the pick in hand,
	/// with a swing wide enough for the toggle to mean anything. The
	/// touch tap and the frame's draw both ask this one predicate.
	pub(super) fn pick_toggle_visible(&self) -> bool {
		self.hotbar.tool_in_hand() == BlockType::ResonantPick
			&& digblock_core::unlock::pick_cells(
				self.skills.level(digblock_core::skills::Skill::Mining),
			) > 1
	}

	/// The tool-ability seam: the bind pressed, or the indicator tapped.
	/// Cycles the pick's dig shape through what the Mining level has
	/// opened; a stowed pick or an unwidened swing no-ops silently (the
	/// indicator is hidden in exactly those states).
	pub(super) fn cycle_dig_orientation(&mut self) {
		use digblock_core::pick::DigOrientation as O;
		let width = digblock_core::unlock::pick_cells(
			self.skills.level(digblock_core::skills::Skill::Mining),
		);
		if width < 3 || self.hotbar.tool_in_hand() != BlockType::ResonantPick {
			return;
		}
		let cur = self.dig_orientation.clamped(width);
		self.dig_orientation = match (cur, width >= digblock_core::unlock::PICK_PLANE) {
			(O::Horizontal, _) => O::Vertical,
			(O::Vertical, true) => O::Plane,
			_ => O::Horizontal,
		};
		// update.rs's per-frame sync carries it to the server; the feed
		// line is for the desktop, whose indicator is small.
		self.push_feed(Msg::PickOrientation(self.dig_orientation));
	}

	/// The keybinding seam: a spell-slot key pressed, or the HUD slot
	/// tapped. Gated on the SAME readiness the spellbook rows enforce; a
	/// refusal speaks in the feed rather than opening the book mid-play.
	pub(super) fn activate_spell_slot(&mut self, i: usize) {
		let Some(id) = self.spell_bar.get(i) else {
			self.push_feed(Msg::SpellSlotEmpty);
			return;
		};
		if self.cast.is_some() {
			return;
		}
		let ports = self.known_ports();
		let cooldown = self.save.teleport_cooldown.left().ceil() as u32;
		let view = crate::ui::screens::spellbook::SpellView::build(
			&self.skills,
			cooldown,
			&ports,
			&self.inventory,
			self.causeway_left.ceil() as u32,
		);
		if let Some(act) = view.action_for(id) {
			self.apply_spell_action(act);
			return;
		}
		let spell = crate::spellbar::voice_spell(id);
		let teleport = id == crate::spellbar::SLOT_HOME || crate::spellbar::port_kind(id).is_some();
		if view.slot_locked(id) {
			self.push_feed(Msg::SpellLocked(spell));
		} else if teleport && cooldown > 0 {
			self.push_feed(Msg::TeleportWait(cooldown));
		} else {
			self.push_feed(Msg::SpellFareShort(spell));
		}
	}

	pub(super) fn start_cast(&mut self, target: CastTarget) {
		if self.cast.is_some() {
			return;
		}
		if target.teleports() && !self.save.teleport_cooldown.ready() {
			self.input.keys = HeldKeys::default();
			self.input.dig_held = false;
			self.ui.spellbook.open();
			return;
		}
		// The Sphere climbs THROUGH the channel rather than after it, so
		// its validation, bill and first cells belong to the start: the
		// server hears `Cast(Sphere)` now and seals on its own clock, and
		// offline the same plan begins its climb beside the channel. A
		// refusal here is a channel that never opens.
		if target == CastTarget::Sphere && !self.begin_sphere() {
			return;
		}
		self.cast = Some(CastState { target, ticks: 0 });
		self.emit(crate::audio::AudioCmd::LoopStart {
			id: crate::audio::LOOP_CAST,
			sound: crate::audio::Sfx::CastLoop,
			pos: None,
			gain: crate::audio::CAST_LOOP_GAIN,
		});
	}

	/// Open a sphere's channel. Online the server is the judge and the client only DECLARES the
	/// slot, but the honest gates run here first so the refusal names what is wrong — an empty
	/// hand, a block no shell can be built of, too few of it — where the wire's reasons are
	/// spell-blind. A plan this end cannot draw is left to the server, whose residency is wider.
	fn begin_sphere(&mut self) -> bool {
		let online = self.session.client.as_ref().is_some_and(|n| n.ready());
		if !online {
			return self.cast_sphere_offline();
		}
		let Some((block, level)) = self.sphere_hand() else {
			return false;
		};
		let r = digblock_core::unlock::sphere_radius(level);
		let head = digblock_core::kindle::host_cell(self.body_pos());
		if let Ok(writes) =
			digblock_core::sphere::plan(head, r, block, |c| self.pipeline.cell_at(c.0, c.1, c.2))
			&& !self.sphere_stocked(block, &writes)
		{
			return false;
		}
		if !self.inventory.can_pay(&sphere_fares(level)) {
			self.push_feed(Msg::SpellFareShort(Spell::Sphere));
			return false;
		}
		let slot = self.hotbar.selected_index() as u8;
		if let Some(net) = self.session.client.as_mut() {
			net.send_cast(digblock_net::messages::CastKind::Sphere { slot });
		}
		true
	}

	/// The rung and the hand, the gates both arms share: the caster's
	/// Arcana admits the spell, and the bar points at a block a shell
	/// can be built of. Speaks the refusal and answers nothing.
	fn sphere_hand(&mut self) -> Option<(BlockType, u32)> {
		use digblock_core::skills::Skill;
		let level = self.skills.level(Skill::Arcana);
		if digblock_core::unlock::sphere_radius(level) == 0 {
			self.push_feed(Msg::SpellLocked(Spell::Sphere));
			return None;
		}
		if self.own_sphere().is_some() {
			self.push_feed(Msg::CastBadTarget);
			return None;
		}
		let Some(HotbarItem::Block(block)) = self.hotbar.selected_item() else {
			self.push_feed(Msg::SphereNoBlock);
			return None;
		};
		if !digblock_core::sphere::accepts(block) {
			self.push_feed(Msg::SphereBadBlock(Some(block)));
			return None;
		}
		Some((block, level))
	}

	/// Does the stock cover what this plan bills? Speaks the shortfall,
	/// naming the count, since a player can go and get exactly that many.
	fn sphere_stocked(
		&mut self,
		block: BlockType,
		writes: &[(digblock_core::sphere::Cell, u16)],
	) -> bool {
		let need = digblock_core::sphere::blocks_needed(writes, block);
		let have = self.inventory.count(block);
		if have < need {
			self.push_feed(Msg::SphereBlocksShort(Some((block, need, have))));
			return false;
		}
		true
	}

	/// The channel broke with a Sphere climbing under it: the server
	/// unwinds on our word (and on its own, if we left); this end puts
	/// its own climb's record back crown-first, only where the block is
	/// still ours, and the bill returns.
	fn break_sphere(&mut self) {
		if let Some(net) = self.session.client.as_mut()
			&& net.ready()
		{
			net.send_cast_break();
		}
		// OUR channel broke, so OUR sphere comes down. A remote's is
		// standing in this same world on its own caster's clock, and
		// walking away from our own cast is nothing to it.
		let me = self.session.my_player_id;
		if let Some(i) = self.sphere_climbs.iter().position(|w| w.own(me)) {
			let climb = self.sphere_climbs.remove(i);
			self.unwind_climb(climb);
		}
	}

	/// Take every climb this world holds back down without telling anyone — ours and each remote's,
	/// since the world under all of them is the one going away. Separate from [`Self::break_sphere`]
	/// because leaving the WORLD is not a broken channel: there is nobody left to send `CastBreak`
	/// to, and only OUR climb would be the one to break. Every path that puts this world down calls
	/// it while that world is still the one under the cells.
	pub(super) fn abandon_sphere(&mut self) {
		for climb in std::mem::take(&mut self.sphere_climbs) {
			self.unwind_climb(climb);
		}
	}

	/// One climb taken back down: the shell comes down crown-first, only where it is still ours —
	/// somebody who dug a block out mid-climb keeps their hole AND the block, since the dig paid
	/// them — and the bill returns: both fares, the blocks never placed and every one just taken
	/// down. Restored from the climb's OWN RECORD, never a re-derived plan: a plan drawn against a
	/// standing sphere names precisely the cells missing from it, and would resurrect what was dug.
	pub(super) fn unwind_climb(&mut self, climb: SphereClimb) {
		let wall = climb.wall();
		let (block, placed, bill) = (climb.block, climb.placed, climb.bill);
		let mut restored = 0;
		for (c, written, prev) in climb.closing.unwind() {
			if self.pipeline.cell_at(c.0, c.1, c.2) == Some(written) {
				if written == wall {
					restored += 1;
				}
				self.write_sphere_cell(c, prev);
			}
		}
		if let Some(bill) = bill {
			self.inventory
				.add(block, bill.debited.saturating_sub(placed) + restored);
			self.inventory.add_fares(&bill.fares);
		}
	}

	/// A climb that reached its crown, ours or a stranger's: the seal's wash and chime, and for
	/// OUR OWN the wisp the spell lights — most matter makes a dark room — with, offline, the bill
	/// settled (the blocks the climb declined come back) and both fares' XP paid. Online the
	/// server lights, settles and pays through its own frames, and this end merely predicts the
	/// light; `SphereEnded` puts it out if the server unwound instead.
	pub(super) fn seal_climb(&mut self, climb: SphereClimb, own: bool) {
		use digblock_core::skills::Skill;
		if own {
			self.light_wisp();
			if let Some(bill) = &climb.bill {
				self.inventory
					.add(climb.block, bill.debited.saturating_sub(climb.placed));
				let xp: u64 = bill
					.fares
					.iter()
					.map(|&(e, n)| digblock_core::skills::cast_xp(e, n))
					.sum();
				self.award_xp_offline(Skill::Arcana, xp);
			}
		}
		self.sphere_sealed(climb.head, own);
	}

	/// Stand our wisp for the rung's span — what a wisp cast does at
	/// completion and what a sphere's seal does for free.
	pub(super) fn light_wisp(&mut self) {
		use digblock_core::skills::Skill;
		let level = self.skills.level(Skill::Arcana);
		self.wisp_left = digblock_core::unlock::wisp_secs(level) as f32;
	}

	/// Our own climb, if one stands. Every answer that belongs to the
	/// LOCAL player — the offline award, the refund, the channel's
	/// seal hold — asks this rather than "is anything climbing",
	/// because a remote's sphere is climbing in our world too.
	pub(super) fn own_sphere(&self) -> Option<&SphereClimb> {
		let me = self.session.my_player_id;
		self.sphere_climbs.iter().find(|w| w.own(me))
	}

	/// Arm the shared teleport clock at the caster's own window. Every
	/// teleport calls this — a spell that moves you and forgets to would
	/// be a spell that outruns the cooldown.
	fn start_teleport_cooldown(&mut self) {
		let level = self.skills.level(digblock_core::skills::Skill::Arcana);
		self.save
			.teleport_cooldown
			.start(digblock_core::unlock::teleport_cooldown_secs(level) as f32);
	}

	/// How long a Sphere's channel is held past its own length while the
	/// caster waits for the seal — the bound on [`Self::sphere_seal_grace`].
	pub(super) const SPHERE_SEAL_GRACE_SECS: f32 = 0.6;

	/// Ticks past its own length that a channel is held open — the Sphere alone, and only
	/// online. `Cast(Sphere)` goes out as the channel BEGINS, so the server's span ends one
	/// upstream latency after the caster's own bar fills, and the channel waits for that seal
	/// rather than completing on a climb still short of its crown.
	/// Offline the seal IS this end's own last tick, so there is nothing to wait for.
	fn sphere_seal_grace(&self, target: CastTarget) -> u32 {
		let waiting = target == CastTarget::Sphere
			&& self.own_sphere().is_some()
			&& self.session.client.as_ref().is_some_and(|n| n.ready());
		if waiting {
			(Self::SPHERE_SEAL_GRACE_SECS / digblock_core::player::TICK) as u32
		} else {
			0
		}
	}

	/// One fixed-rate step of the cast channel. A caster may walk while
	/// channelling; a dig hold or a surface opening resets the channel
	/// outright — the decay mold is a dig's, deliberately not borrowed.
	pub(super) fn step_cast(&mut self) {
		let Some(&CastState { target, ticks }) = self.cast.as_ref() else {
			return;
		};
		let surface = self.ui.menu.is_open()
			|| self.ui.inv_screen.is_open()
			|| self.ui.alch_screen.is_open()
			|| self.ui.char_screen.is_open()
			|| self.ui.hiscores.is_open()
			|| self.ui.spellbook.is_open()
			|| self.ui.chat.is_open();
		let need = digblock_core::unlock::channel_ticks(
			self.skills.level(digblock_core::skills::Skill::Arcana),
		);
		if surface || self.dig_active() {
			self.cast = None;
			self.emit(crate::audio::AudioCmd::LoopStop {
				id: crate::audio::LOOP_CAST,
			});
			if target == CastTarget::Sphere {
				// Past the channel's own length with nothing climbing,
				// the sphere has already sealed — the seal arrives
				// through the sphere machinery, not the channel's — so
				// walking away now breaks nothing and says nothing.
				if ticks >= need && self.own_sphere().is_none() {
					return;
				}
				self.break_sphere();
			}
			self.push_feed(Msg::CastInterrupted);
			return;
		}
		let ticks = {
			let cast = self.cast.as_mut().expect("checked above");
			cast.ticks += 1;
			cast.ticks
		};
		if ticks < need + self.sphere_seal_grace(target) {
			return;
		}
		self.cast = None;
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_CAST,
		});
		self.complete_cast(target);
	}

	/// The ports this world offers: online, exactly the sites its server
	/// RESOLVED (a port to nowhere must not exist); offline, the whole
	/// ladder — a kind this seed never generated refuses at the cast.
	pub(super) fn known_ports(&self) -> Vec<crate::ui::screens::spellbook::Port> {
		let all = crate::ui::screens::spellbook::all_ports();
		if self.session.client.is_some() {
			all.into_iter()
				.filter(|p| self.session.waystones.iter().any(|&(i, _)| i == p.idx))
				.collect()
		} else {
			all
		}
	}

	/// The offline arm of a waystone cast: the same tables the server validates with, the same
	/// debit, and the site materializes on first visit — stamped, protected and marked, what a
	/// server world did at boot. The cast pays the roster's walk up to its own rung (pure ring
	/// searches; the ladder resolves in order because the separation rule is mutual), kept for the
	/// session. The XP is the fare's worth and nothing else, exactly as a server pays.
	fn cast_waystone_offline(&mut self, idx: u8) {
		use digblock_core::skills::Skill;
		use digblock_core::worldgen::SurfaceKind;
		let Some(&kind) = SurfaceKind::ALL.get(idx as usize) else {
			return;
		};
		let Some(level) = digblock_core::unlock::waystone(kind) else {
			return;
		};
		if self.skills.level(Skill::Arcana) < level {
			self.push_feed(Msg::SpellLocked(Spell::Waystone));
			return;
		}
		let Some(eye) = self.save.offline_waystones.site(idx) else {
			// This seed never made such a place — a port to nowhere.
			self.push_feed(Msg::SpellLocked(Spell::Waystone));
			return;
		};
		let (essence, fare) =
			digblock_core::alchemy::cast_price(kind).expect("a laddered kind has a fare");
		if !self.inventory.try_take_sub(essence, fare) {
			self.push_feed(Msg::SpellFareShort(Spell::Waystone));
			return;
		}
		let region = digblock_core::spawn::lock_region(eye);
		if !self.protected.contains(&region) {
			self.protected.push(region);
			self.pending_stamp
				.extend(digblock_core::waystones::stamp(eye));
			self.vfx.marker_sites.push(Vec3::new(
				eye.x,
				eye.y - digblock_core::player::EYE_TO_FEET,
				eye.z,
			));
		}
		self.apply_teleport(eye, Vec3::ZERO);
		// Offline arm ONLY — the shared path never awards.
		self.award_xp_offline(Skill::Arcana, digblock_core::skills::cast_xp(essence, fare));
	}

	/// A completed channel fires its spell. The teleport that answers —
	/// this frame offline, a round trip later online — is what words the
	/// arrival, via `spawn_return_pending`.
	fn complete_cast(&mut self, target: CastTarget) {
		use digblock_core::skills::Skill;
		// Which prediction a `CastRefused` is answering: the wire carries
		// a reason and never a spell.
		self.last_cast = Some(target);
		match target {
			CastTarget::Home => {
				self.start_teleport_cooldown();
				self.save.spawn_return_pending = true;
				match self.session.client.as_mut() {
					Some(net) if net.ready() => net.send_return_to_spawn(),
					_ => {
						// Same rule the server runs: relocate sideways out of anything dug
						// into the spawn, never descend into it. The overlay — not
						// `pipeline.cell_at` — is the edit view: the pipeline answers None
						// for unloaded columns, which the support test would read as void.
						let from = self.player.pos;
						let pos = self.local_spawn();
						self.apply_teleport(pos, Vec3::ZERO);
						// The offline award arm ONLY — the shared path
						// never awards, or multiplayer double-pays.
						self.award_xp_offline(
							Skill::Arcana,
							digblock_core::skills::home_xp(from.distance(pos)),
						);
					}
				}
			}
			CastTarget::Waystone(idx) => {
				// A port rests on the same clock Return Home does, and
				// the server enforces its own copy of it.
				self.start_teleport_cooldown();
				match self.session.client.as_mut() {
					// The server validates the real gates — the unlock,
					// the clock and the fare — and answers Teleported or
					// CastRefused.
					Some(net) if net.ready() => {
						net.send_cast(digblock_net::messages::CastKind::Waystone(idx));
					}
					_ => self.cast_waystone_offline(idx),
				}
			}
			CastTarget::Wisp => {
				let level = self.skills.level(Skill::Arcana);
				let (essence, fare) = digblock_core::alchemy::wisp_price(level);
				match self.session.client.as_mut() {
					Some(net) if net.ready() => {
						// Predicted: the light shows now, the debit rides
						// the rebase, and a CastRefused puts it out.
						net.send_cast(digblock_net::messages::CastKind::Wisp);
						self.light_wisp();
					}
					_ => {
						if !self.inventory.try_take_sub(essence, fare) {
							self.push_feed(Msg::SpellFareShort(Spell::Wisp));
							return;
						}
						self.light_wisp();
						self.award_xp_offline(
							Skill::Arcana,
							digblock_core::skills::cast_xp(essence, fare),
						);
					}
				}
			}
			CastTarget::Causeway => {
				let level = self.skills.level(Skill::Arcana);
				let (essence, fare) = digblock_core::alchemy::causeway_price(level);
				let secs = digblock_core::unlock::causeway_secs(level) as f32;
				match self.session.client.as_mut() {
					Some(net) if net.ready() => {
						// Predicted like the wisp: the road starts laying
						// now, the debit rides the rebase, and a
						// CastRefused takes it up again.
						net.send_cast(digblock_net::messages::CastKind::Causeway);
						self.causeway_left = secs;
					}
					_ => {
						if !self.inventory.try_take_sub(essence, fare) {
							self.push_feed(Msg::SpellFareShort(Spell::Causeway));
							return;
						}
						self.causeway_left = secs;
						self.award_xp_offline(
							Skill::Arcana,
							digblock_core::skills::cast_xp(essence, fare),
						);
					}
				}
			}
			CastTarget::Kindle => {
				// Not predicted: the server answers every cast with a
				// `Kindled` for everyone, the caster included, so a local
				// copy would stand twice.
				match self.session.client.as_mut() {
					Some(net) if net.ready() => {
						net.send_cast(digblock_net::messages::CastKind::KindleHere);
					}
					_ => self.cast_kindle_offline(),
				}
			}
			CastTarget::Sphere => {
				// Nothing: the climb began with the channel (`begin_sphere`)
				// and the seal is the climb's own last tick (`seal_climb`),
				// which is where the wisp lights and the offline award and
				// settlement happen. Online the server declares both ends —
				// `SphereWinding` stands the sphere up, `SphereEnded` closes
				// or unwinds it — and the XP rides `XpGain`.
			}
		}
	}

	/// Stand a kindled light in `cell` for `secs` — the server's word
	/// online, the offline arm's own clock otherwise. The key is minted
	/// here so every light, whoever lit it, is its own carrier.
	pub(super) fn kindle_light(&mut self, cell: (i32, i32, i32), secs: f32) {
		let key = self.vfx.kindle_seq & carried::KINDLED_MASK;
		self.vfx.kindle_seq = self.vfx.kindle_seq.wrapping_add(1);
		self.vfx.kindled.push(KindledLight {
			cell,
			left: secs,
			age: 0.0,
			key,
		});
	}

	/// The offline arm of conjured light: the same rung, host and fare
	/// rules the server runs, the light standing on the same clock.
	fn cast_kindle_offline(&mut self) {
		use digblock_core::skills::Skill;
		let level = self.skills.level(Skill::Arcana);
		if level < digblock_core::unlock::KINDLE_AT {
			self.push_feed(Msg::SpellLocked(Spell::Kindle));
			return;
		}
		// `body_pos`, not `player.pos`: fly leaves the simulated body
		// behind, and a flying caster's light must stand where they
		// actually are rather than where they last walked.
		let cell = digblock_core::kindle::host_cell(self.body_pos());
		if !digblock_core::kindle::hosts(self.pipeline.cell_at(cell.0, cell.1, cell.2)) {
			self.push_feed(Msg::CastBadTarget);
			return;
		}
		let (essence, fare) = digblock_core::alchemy::kindle_price(level);
		if !self.inventory.try_take_sub(essence, fare) {
			self.push_feed(Msg::SpellFareShort(Spell::Kindle));
			return;
		}
		self.kindle_light(cell, digblock_core::kindle::SECS as f32);
		self.award_xp_offline(Skill::Arcana, digblock_core::skills::cast_xp(essence, fare));
	}

	/// The offline arm of the Sphere's START: the same rung, hand, plan,
	/// player scan and bill the server runs, protected cells skipped the
	/// same way — here BEFORE the bill, so a cell the sphere will never
	/// write is never charged for; what it leaves behind is a climb the
	/// tick loop steps beside the channel. False when nothing began.
	fn cast_sphere_offline(&mut self) -> bool {
		let Some((block, level)) = self.sphere_hand() else {
			return false;
		};
		let r = digblock_core::unlock::sphere_radius(level);
		// The HEAD is the same position the shell is scanned against
		// (`body_pos`), or a flying caster's sphere closes around where
		// they last stood while the scan clears them where they are.
		let head = digblock_core::kindle::host_cell(self.body_pos());
		let Ok(writes) =
			digblock_core::sphere::plan(head, r, block, |c| self.pipeline.cell_at(c.0, c.1, c.2))
		else {
			self.push_feed(Msg::CastBadTarget);
			return false;
		};
		if writes
			.iter()
			.any(|&(c, v)| digblock_core::player::placement_blocked(self.body_pos(), c, v))
		{
			self.push_feed(Msg::CastBadTarget);
			return false;
		}
		let mut writes = writes;
		writes.retain(|&(c, _)| !digblock_core::protect::immutable(c.0, c.1, c.2, &self.protected));
		if !self.sphere_stocked(block, &writes) {
			return false;
		}
		let fares = sphere_fares(level);
		if !self.inventory.try_take_fares(&fares) {
			self.push_feed(Msg::SpellFareShort(Spell::Sphere));
			return false;
		}
		let debited = digblock_core::sphere::blocks_needed(&writes, block);
		let took = self.inventory.try_take(block, debited);
		debug_assert!(took, "sphere_stocked answered for this");
		self.sphere_climbs.push(SphereClimb {
			by: self.session.my_player_id,
			closing: digblock_core::sphere::Closing::new(
				writes,
				digblock_core::unlock::channel_ticks(level),
			),
			head,
			block,
			prefix: 0,
			placed: 0,
			bill: Some(SphereBill { debited, fares }),
		});
		true
	}

	/// One sim tick of every sphere climbing — ours and each remote's,
	/// each at its own caster's pace, so hundreds of edits ride one
	/// frame from the wire. The tick a climb ends is its seal.
	/// DIG_SPHERE_AGE holds a climb at its age so a capture can
	/// photograph the bowl.
	pub(super) fn step_sphere_climbs(&mut self) {
		if self.sphere_climbs.is_empty() {
			return;
		}
		let me = self.session.my_player_id;
		// Taken out for the walk: landing a cell wants the whole engine,
		// and a climb removed from the list is not a borrow of it.
		let mut climbs = std::mem::take(&mut self.sphere_climbs);
		let mut i = 0;
		while i < climbs.len() {
			if let Some(age) = self.config.debug_sphere_age
				&& climbs[i].closing.step() as f32 * digblock_core::player::TICK >= age
			{
				i += 1;
				continue;
			}
			self.land_sphere_tick(&mut climbs[i]);
			if climbs[i].closing.done() {
				let climb = climbs.remove(i);
				let own = climb.own(me);
				self.seal_climb(climb, own);
				continue;
			}
			i += 1;
		}
		self.sphere_climbs = climbs;
	}

	/// One tick's worth of a climb handed to the world and recorded with what it replaced, so a
	/// break can put it back. A cell somebody has stepped into is left unwritten — the server's
	/// rule too — and so is one this end cannot read, which the journal carries instead. Neither is
	/// recorded, so no unwind claims a cell this end never wrote.
	fn land_sphere_tick(&mut self, climb: &mut SphereClimb) {
		let body = self.body_pos();
		let wall = climb.wall();
		let cells = climb.closing.land();
		climb.prefix += cells.len();
		for (c, v) in cells {
			if digblock_core::player::placement_blocked(body, c, v) {
				continue;
			}
			let Some(prev) = self.pipeline.cell_at(c.0, c.1, c.2) else {
				continue;
			};
			climb.closing.record(c, v, prev);
			if v == wall {
				climb.placed += 1;
			}
			self.write_sphere_cell(c, v);
		}
	}

	/// Walk a climb forward to a plan PREFIX — the server's own count of what it handed the world,
	/// and the truth about a sphere whose local clock is a round trip out of step either way. A
	/// tick-sized step can overshoot the count by one tick's cells; only a break can leave those
	/// standing, and a break takes the whole record back down.
	pub(super) fn land_sphere_to(&mut self, climb: &mut SphereClimb, prefix: usize) {
		while climb.prefix < prefix && !climb.closing.done() {
			self.land_sphere_tick(climb);
		}
	}

	/// Hand one of a sphere's cells to the world through the arm that owns this world's edits:
	/// offline `set_cell`, the local write funnel a save's overlay is recorded from; online
	/// `apply_authoritative`, the path every journalled edit takes. Online `set_cell` records
	/// nothing, so a sphere written that way is gone from the overlay mirroring the journal and
	/// vanishes on the first hard eviction.
	pub(super) fn write_sphere_cell(&mut self, c: digblock_core::sphere::Cell, v: u16) {
		if self.session.client.is_some() {
			self.apply_remote_edit(c, v);
		} else {
			self.pipeline.set_cell(c.0, c.1, c.2, v);
		}
	}

	/// Mouse hold or touch hold — the one question the dig machinery asks.
	/// Touch is deliberately NOT folded into `dig_held`: that flag is
	/// force-cleared at many sites (screen opens, focus loss, slot swaps)
	/// whose touch half the driver's own `clear` already covers, and two
	/// writers to one flag would have to agree at every one of them.
	pub(super) fn dig_active(&self) -> bool {
		self.input.dig_held || self.input.touch.dig_held()
	}

	/// One fixed-rate step of the hold-to-dig state machine. Purely
	/// predictive online — the server runs its own timer and aim checks.
	pub(super) fn step_dig(&mut self) {
		// A charged bucket has no left-click action at all — not even the
		// "you can't dig that" complaint below, which belongs to a dig.
		if !self.dig_active() || (self.water_tool_selected() && !self.scoop_ready()) {
			// The button coming up is what arms the drawn cell again: the
			// latch exists to break ONE press into two, so it can only be
			// cleared by the press ending.
			self.drawn_cell = None;
			self.release_dig();
			return;
		}
		// Aiming somewhere new clears the refusal, so a fresh attempt on
		// a different block speaks again.
		if self.refused_cell.is_some() && self.refused_cell != self.target.map(|h| h.cell) {
			self.refused_cell = None;
		}
		// The locally-knowable refusals (the world floor, the two skill
		// gates). A server-side lock can't be predicted and arrives as
		// `DigRefused` instead. The latch keeps each refusal to one feed
		// line per aim, same as the server path.
		if let Some((cell, msg)) = self.target.and_then(|h| {
			if digblock_core::protect::immutable(h.cell.0, h.cell.1, h.cell.2, &self.protected) {
				Some((h.cell, Msg::DigProtected))
			} else if self.node_dig_mode(h.cell)
				== digblock_core::worldgen::nodes::DigMode::NeedsSiphon
			{
				Some((h.cell, Msg::NeedsSiphon))
			} else {
				digblock_core::unlock::dig_blocked(h.block, &self.skills)
					.map(|req| (h.cell, Msg::DigRefused(req)))
			}
		}) && self.refused_cell != Some(cell)
		{
			self.refused_cell = Some(cell);
			self.push_feed(msg);
		}
		let valid = self.target.filter(|h| self.diggable(h));
		let Some(hit) = valid else {
			self.release_dig();
			return;
		};
		// The seam this press already drew: the spent rock standing where
		// it was is not what the player was aiming at, so this press does
		// not get to break it too. Aiming off it and back re-arms nothing
		// — only the button coming up does — but any OTHER cell, seam or
		// stone, digs from here as it always did.
		if self.drawn_cell == Some(hit.cell) {
			self.release_dig();
			return;
		}
		// Three cases, and only the first advances a tick this step: an
		// already-live hold on this exact cell, a DECAYING one on it
		// (resumed, inheriting whatever the decay left), or a fresh one.
		let same = self
			.dig
			.as_ref()
			.filter(|d| d.cell == hit.cell && d.cell_value == hit.cell_value)
			.map(|d| (d.idle == 0, d.banked()));
		if !same.is_some_and(|(live, _)| live) {
			// A resume keeps its banked progress; anything else drops what it was holding
			// (and tells the server, which stops it verifying aim on a cell we walked away
			// from). Either way the hold re-opens from here, DigStart included — the server
			// was told to stop the moment the button came up, so a resume has to ask it to
			// start counting again.
			let carried = match same {
				Some((_, banked)) => banked,
				None => {
					self.cancel_dig();
					0
				}
			};
			// Family-scaled hold time (SIM v12) — the same `dig_required` the server's
			// completion gate runs. An EXTRACTION is timed as the ORE rather than as the
			// stone the cell still holds, so a deep seam takes longer than a shallow one and
			// the ore rows' hardnesses are not dead numbers on blocks nobody digs.
			let mode = self.node_dig_mode(hit.cell);
			let timed = digblock_core::worldgen::nodes::dig_block(mode, hit.block);
			let required = timed
				.dig_hardness(hit.cell.1)
				.map(|h| {
					digblock_core::skills::dig_required_with(
						timed,
						h,
						self.skills.level(digblock_core::skills::Skill::Mining),
						self.hotbar.tool_in_hand(),
					) as u16
				})
				.unwrap_or(u16::MAX);
			self.dig = Some(DigProgress {
				cell: hit.cell,
				cell_value: hit.cell_value,
				ticks: carried,
				required,
				idle: 0,
			});
			// The hand this hold is made with rides along: the server's
			// tool gates read it, and the input path that otherwise
			// carries the selection is jitter-buffered behind this frame.
			let slot = self.hotbar.selected_index() as u8;
			if let Some(net) = self.session.client.as_mut()
				&& net.ready()
			{
				net.send_dig_start(hit.cell, slot);
			}
			// A scoop is a swing, not a mining hold: one stroke, no
			// grinding loop. (Water has no sound material of its own, so
			// the loop resolved to `Material::Earth` and collecting a
			// spring played the dirt-digging sample.) The arc starts here,
			// with the hold it measures — its length is the scoop's, so
			// the hand comes back up exactly as the water lands in it.
			if self.water_tool_selected() {
				self.vfx.view_swing.strike();
				self.react(crate::events::GameEvent::WaterScooped { cell: hit.cell });
			} else {
				// A draw rides the SAME voice as a dig, so every stop path already covers
				// it, and the pitch ramp below reads as a resonance winding up rather than
				// as a pick speeding up. WHICH sound it is belongs to `audio::dig_voice`,
				// together with the finish it has to agree with.
				let extracting =
					matches!(mode, digblock_core::worldgen::nodes::DigMode::Extract(_));
				self.emit(crate::audio::AudioCmd::LoopStart {
					id: crate::audio::LOOP_DIG,
					sound: crate::audio::dig_voice(extracting, hit.block).0,
					pos: Some(cell_center(hit.cell)),
					gain: if extracting {
						crate::audio::DRAW_LOOP_GAIN
					} else {
						crate::audio::DIG_LOOP_GAIN
					},
				});
			}
			return;
		}
		let (done, pitch) = {
			let d = self.dig.as_mut().expect("live hold checked");
			d.ticks += 1;
			// Pitch ramp on a subdivision of the tick: the loop climbs
			// toward completion.
			let pitch = (d.ticks.is_multiple_of(6) && d.required > 0)
				.then(|| 1.0 + 0.5 * d.ticks as f32 / d.required as f32);
			(d.ticks >= d.required, pitch)
		};
		if let Some(rate) = pitch {
			self.emit(crate::audio::AudioCmd::LoopPitch {
				id: crate::audio::LOOP_DIG,
				rate,
			});
		}
		if done {
			self.dig = None;
			self.complete_dig(hit);
		}
	}

	/// Stop digging, but KEEP what the hold banked: progress bleeds back over
	/// `skills::DIG_DECAY_TICKS`, and a hold resumed inside that window inherits what is left.
	/// Called once per sim step while not digging — that repetition advances the decay, and
	/// dropping the record at zero keeps a spent hold from being resumed for free. The server runs
	/// the identical decay from ITS release: only JITTER separates them, which `dig_grace` absorbs.
	pub(super) fn release_dig(&mut self) {
		let Some(d) = self.dig.as_mut() else {
			return;
		};
		let stopping = d.idle == 0;
		d.idle = d.idle.saturating_add(1);
		if d.banked() == 0 {
			self.dig = None;
		}
		if stopping {
			if let Some(net) = self.session.client.as_mut()
				&& net.ready()
			{
				net.send_dig_cancel();
			}
			self.emit(crate::audio::AudioCmd::LoopStop {
				id: crate::audio::LOOP_DIG,
			});
		}
	}

	/// Discard any in-progress hold outright, decay and all — a retarget
	/// onto a different cell, a world swap, or a server refusal. The soft
	/// path (button up, target momentarily lost) is `release_dig`.
	pub(super) fn cancel_dig(&mut self) {
		if self.dig.take().is_some() {
			if let Some(net) = self.session.client.as_mut()
				&& net.ready()
			{
				net.send_dig_cancel();
			}
			self.emit(crate::audio::AudioCmd::LoopStop {
				id: crate::audio::LOOP_DIG,
			});
		}
	}

	/// The hold finished: apply the predicted dig/collect and credit the
	/// drop. The server confirms (no-op) or rejects (cell + credit revert).
	/// Digging either half of a door clears BOTH cells under one seq.
	pub(super) fn complete_dig(&mut self, hit: RayHit) {
		let air = BlockType::Air as u16;
		let Some(prev_cell) = self.pipeline.cell_at(hit.cell.0, hit.cell.1, hit.cell.2) else {
			return;
		};
		let prev = BlockType::from_id(digblock_core::chunk::cell_id(prev_cell));
		// (cell, prev) list this dig clears — a door adds its partner, the Resonant Pick adds
		// the plane of the face (SKILLS-DESIGN §5.6). Every extra cell holds the SAME id and is
		// the plain breakable case (`pick::breaks_as_extra`), which is what makes `begin_multi`'s
		// single `inv_delta` a correct description of the whole group — and what keeps the aimed
		// door's own partner out of the plane, where it would be counted AND written twice.
		let mut cleared = vec![(hit.cell, prev_cell)];
		for c in self.pick_plane(&hit) {
			if let Some(v) = self.pipeline.cell_at(c.0, c.1, c.2) {
				cleared.push((c, v));
			}
		}
		// How many cells this ONE hold broke — one normally, a whole plane
		// with the pick, and the count `begin_multi`'s single `inv_delta`
		// carries for the whole group.
		let broken = cleared.len() as u32;
		if prev.is_door() {
			let partner = digblock_core::shapes::door_partner(
				hit.cell,
				digblock_core::chunk::cell_state(prev_cell),
			);
			if let Some(pc) = self.pipeline.cell_at(partner.0, partner.1, partner.2)
				&& BlockType::from_id(digblock_core::chunk::cell_id(pc)).is_door()
			{
				cleared.push((partner, pc));
			}
		}
		// A field extraction (SKILLS-DESIGN §5.3): the vein is DRAWN out,
		// not dug out, so the cell becomes a tapped face and the mountain
		// keeps its shape. Resolved before the write, because it changes
		// both what is written and what is credited.
		let extracting = match self.node_dig_mode(hit.cell) {
			digblock_core::worldgen::nodes::DigMode::Extract(node) => Some(node),
			_ => None,
		};
		// What the dig pays, resolved before the write for the same reason
		// and one of its own: in a LOCAL world this engine is the authority,
		// so `set_cell` records the coordinate itself — asking afterwards
		// would find every dig standing in an edited cell and hand back
		// clean stone for a quarried one.
		let credit = prev.dig_credit(
			self.cell_edited(hit.cell),
			self.skills.level(digblock_core::skills::Skill::Alchemy),
		);
		// Slabs shrink one third per completed dig (2/3 → 1/3 → Air);
		// everything else clears. Door partners always clear.
		let new_val = if prev.is_slab() {
			digblock_core::shapes::slab_shrunk(prev_cell)
		} else if let Some(node) = extracting {
			digblock_core::worldgen::nodes::spent_block(node).expect("an Extract names a node kind")
				as u16
		} else {
			air
		};
		if self
			.pipeline
			.set_cell(hit.cell.0, hit.cell.1, hit.cell.2, new_val)
			.is_none()
		{
			return;
		}
		for &(c, _) in &cleared[1..] {
			self.pipeline.set_cell(c.0, c.1, c.2, air);
		}
		self.emit(crate::audio::AudioCmd::LoopStop {
			id: crate::audio::LOOP_DIG,
		});
		// A finished draw flares and then dies on the ordinary fade-out
		// rather than blinking off with the seam it emptied. What it
		// flares FROM is already banked in `SiphonDraw`, which is the whole
		// reason that state outlives the hold: the cell is spent rock by
		// the write above, so there is no vein left to ask.
		if extracting.is_some()
			&& let Some(s) = self.vfx.siphon.as_mut()
		{
			s.progress = 1.0;
			s.envelope = crate::siphon::FLARE;
		}
		// And the cell is latched until the button comes up. A draw is one
		// action; the spent rock it leaves is another, and a press that
		// rolled from the first into the second quarried away the trophy
		// on the same hold that earned it.
		if extracting.is_some() {
			self.drawn_cell = Some(hit.cell);
		}
		// How the hold ends — scoop splash, seam release, or the break —
		// is the sound table's business (`audio::sound_for`).
		let tool = self.water_tool_selected();
		self.react(crate::events::GameEvent::DigFinished {
			cell: hit.cell,
			block: prev,
			extracted: extracting.is_some(),
			scooped: tool,
		});
		// No swing is started here: the arc that began with this hold is running out on this
		// frame, and re-striking it bolts a second stroke onto a finished scoop. An extraction
		// credits NOTHING here — a drawn seam pays essence as a SPLIT, which a `PendingEdit`'s
		// single `inv_delta` cannot carry, so it is deliberately un-predicted and the server's
		// `InventoryFull` rebase answers. The CELL is predicted, so the vein turns spent in hand.
		let drop = if extracting.is_some() { None } else { credit };
		if let Some(d) = drop {
			self.inventory.add(d, broken);
			self.hotbar.auto_assign(d);
			self.nudge_siphon();
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		match self.session.client.as_mut() {
			Some(net) if net.ready() => {
				// `tool` is `water_tool_selected` — the SLOT is not enough. It is the same
				// predicate `diggable` and the scoop ray use, and the same one the server's
				// Collect gate asks (`slot_item == WaterTool && active_tool == Bucket`).
				// Reading the slot alone calls every hold with another tool in hand a
				// Collect, which the server answers WrongSlot: cell and credit revert.
				let kind = if tool {
					digblock_net::messages::EditKind::Collect
				} else {
					digblock_net::messages::EditKind::Dig
				};
				let tick = net.predictor.current_tick();
				let entries: Vec<_> = cleared
					.iter()
					.enumerate()
					.map(|(i, &(c, p))| (c, p, if i == 0 { new_val } else { air }))
					.collect();
				let seq = net
					.edits
					.begin_multi(&entries, tick, drop.map(|d| (d, broken as i32)));
				let slot = self.hotbar.selected_index() as u8;
				net.send_edit(seq, tick, origin, dir, kind, hit.cell, (0, 0, 0), slot);
			}
			_ => {
				for &(c, _) in &cleared {
					self.fluid.schedule_around(c.0, c.1, c.2);
				}
				// Offline, the engine IS the authority — for the essence a
				// drawn seam pays as much as for the XP, and both go
				// through the same shared functions the server's
				// chokepoint calls, so §5.2's depth reward, §5.4's dual
				// award at the rock face and §5.3's field yield reach both
				// economies identically.
				if let Some(ore) = extracting {
					let mining = self.skills.level(digblock_core::skills::Skill::Mining);
					let alchemy = self.skills.level(digblock_core::skills::Skill::Alchemy);
					for (e, n) in digblock_core::alchemy::field_yield(
						ore,
						alchemy,
						mining,
						hit.cell,
						self.seed.value,
					) {
						self.inventory.add_sub(e, n);
						self.toast(crate::ui::hud::ToastLabel::Essence(e), u64::from(n));
					}
				}
				let (mining_xp, alch_xp) =
					digblock_core::skills::dig_award(prev, extracting, broken, hit.cell.1);
				self.award_xp_offline(digblock_core::skills::Skill::Mining, mining_xp);
				if alch_xp > 0 {
					self.award_xp_offline(digblock_core::skills::Skill::Alchemy, alch_xp);
				}
			}
		}
	}

	/// Resolve a pending right-click: USE beats PLACE — a door under the
	/// crosshair toggles; otherwise placement against the target, gated
	/// on stock ("you can only place what you gathered").
	pub(super) fn apply_place(&mut self) {
		if !std::mem::take(&mut self.input.click_place) {
			return;
		}
		let Some(hit) = self.target else { return };
		// Use: the pick ray narrow-phases against interaction boxes, so
		// this finds an OPEN door on its swung leaf too — there is no
		// second ray to walk.
		if hit.block.is_door() {
			self.toggle_door(hit);
			return;
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		if hit.normal == (0, 0, 0) {
			return;
		}
		let block = match self.hotbar.selected_item() {
			Some(HotbarItem::Block(b)) => b,
			// The charged bucket pours its one source. It works here, and only here, because
			// a charged bucket picks with the ORDINARY ray: the fluid ray stops INSIDE a
			// source, leaving no face normal to build against. The BUCKET, not the slot —
			// the server's place gate requires `active_tool == Bucket`, so pouring with the
			// pick or siphon active answers WrongSlot and the source appears and vanishes.
			Some(HotbarItem::WaterTool)
				if self.hotbar.active_tool() == Some(BlockType::Bucket)
					&& self.inventory.bucket_full() =>
			{
				BlockType::Water
			}
			// An empty bucket has nothing to give. Doors above still toggle.
			Some(HotbarItem::WaterTool) | None => return,
		};
		// Stairs carry the placer-derived facing (ascending away along
		// the look); doors place as a lower+upper pair.
		use digblock_core::chunk::{cell_id, make_cell};
		use digblock_core::shapes::{
			door_state, slab_grow_normal, slab_grown, slab_place_state, stair_state,
		};
		// A slab onto a same-material slab's OPEN face — the top of a floor slab, the
		// underside of a ceiling one — grows the hit cell in place instead of writing the
		// neighbor. The wire still carries the canonical state-0 slab: the server re-derives
		// both the grow and the anchor from its own raycast.
		let grow = block.is_slab()
			&& cell_id(hit.cell_value) == block as u16
			&& hit.normal == slab_grow_normal(hit.cell_value);
		let (target, placed_cell) = if grow {
			(hit.cell, slab_grown(hit.cell_value))
		} else {
			let target = (
				hit.cell.0 + hit.normal.0,
				hit.cell.1 + hit.normal.1,
				hit.cell.2 + hit.normal.2,
			);
			// Stairs and furniture with a front carry their facing in the
			// same state field, so one branch turns both — and a new
			// facing shape cannot forget to be turned at all.
			let placed = if block.takes_facing() {
				make_cell(block, stair_state(quantized_facing(dir)))
			} else if block.is_door() {
				make_cell(block, door_state(quantized_facing(dir), false, false))
			} else if block.is_slab() {
				// A fresh 1/3, hanging from the ceiling it was built
				// against or sitting on anything else.
				make_cell(block, slab_place_state(hit.normal))
			} else {
				block as u16
			};
			(target, placed)
		};
		// The target must be loaded and replaceable — same core predicate the
		// server validates with, so a predicted place is never one the
		// server then rejects. `grow` skips this: it rewrites the hit cell
		// itself, which is a slab, not a free cell.
		if !grow && !self.cell_is_replaceable(target) {
			return;
		}
		// Never place into the player — the shared shape-aware contract
		// (identical to the server's InsidePlayer scan). Doors check
		// both halves.
		let mut placements = vec![(target, placed_cell)];
		if block.is_door() {
			let upper = (target.0, target.1 + 1, target.2);
			// The upper cell, same rule (mirrors the server's gate).
			if !self.cell_is_replaceable(upper) {
				return;
			}
			placements.push((
				upper,
				make_cell(block, door_state(quantized_facing(dir), false, true)),
			));
		}
		// Protected cells, scanned over the ASSEMBLED placements so a
		// door's derived upper half and a slab grow (which rewrites the
		// hit cell, not the one past its face) are both covered — the
		// mirror of the server's scan over `writes`. Until now the only
		// thing between a place and the world floor was
		// `cell_is_replaceable`, which asks about block IDENTITY, and the
		// floor rule is the one rule that must never depend on what
		// happens to be standing there. Offline the arrival regions ride
		// along; a SERVER's regions arrive as a rejection instead.
		if placements
			.iter()
			.any(|&(c, _)| digblock_core::protect::immutable(c.0, c.1, c.2, &self.protected))
		{
			self.push_feed(Msg::PlaceProtected);
			return;
		}
		if placements
			.iter()
			.any(|&(c, v)| digblock_core::player::placement_blocked(self.body_pos(), c, v))
		{
			return;
		}
		// Stock gate BEFORE the world mutation: no stock, no prediction.
		// One debit covers a door's two cells.
		if !self.inventory.try_take(block, 1) {
			return;
		}
		let mut entries = Vec::new();
		for &(c, v) in &placements {
			match self.pipeline.set_cell(c.0, c.1, c.2, v) {
				Some(prev) => entries.push((c, prev, v)),
				None if entries.is_empty() => {
					self.inventory.add(block, 1); // refund: nothing happened
					return;
				}
				None => {}
			}
		}
		self.react(crate::events::GameEvent::Placed {
			cell: target,
			poured: block == BlockType::Water,
		});
		match self.session.client.as_mut() {
			Some(net) if net.ready() => {
				let tick = net.predictor.current_tick();
				let seq = net.edits.begin_multi(&entries, tick, Some((block, -1)));
				// Slabs always travel as the canonical state-0 cell: the
				// server derives the grow AND the anchor from the normal
				// it verified, so neither is a state a client can assert.
				let wire_cell = if grow || block.is_slab() {
					block as u16
				} else {
					placed_cell
				};
				net.send_edit(
					seq,
					tick,
					origin,
					dir,
					digblock_net::messages::EditKind::Place { block: wire_cell },
					hit.cell,
					hit.normal,
					self.hotbar.selected_index() as u8,
				);
			}
			_ => {
				for &(c, _, _) in &entries {
					self.fluid.schedule_around(c.0, c.1, c.2);
				}
				// A sealing cover starves the grass beneath it back to
				// dirt (`grass::smothered_by`) — the engine's arm of the
				// server's post-place consequence. Deliberately NOT
				// predicted in multiplayer: there it arrives as the
				// server's own broadcast edit, so a rejected place never
				// has a companion write to unwind.
				for &(c, _, v) in &entries {
					let below = (c.0, c.1 - 1, c.2);
					if digblock_core::protect::immutable(below.0, below.1, below.2, &self.protected)
					{
						continue;
					}
					let Some(dirt) = self
						.pipeline
						.cell_at(below.0, below.1, below.2)
						.and_then(|cur| digblock_core::worldjob::grass::smothered_by(v, cur))
					else {
						continue;
					};
					self.pipeline.set_cell(below.0, below.1, below.2, dirt);
					self.fluid.schedule_around(below.0, below.1, below.2);
				}
			}
		}
	}

	/// Predict a door toggle on both halves and send the Interact (the
	/// server re-raycasts and flips authoritatively). Offline it IS the
	/// authority. Never consumes stock.
	pub(super) fn toggle_door(&mut self, hit: RayHit) {
		use digblock_core::chunk::{cell_id, cell_state};
		use digblock_core::shapes::{door_partner, door_toggled};
		let cell = hit.cell;
		let Some(cur) = self.pipeline.cell_at(cell.0, cell.1, cell.2) else {
			return;
		};
		if !BlockType::from_id(cell_id(cur)).is_door() {
			return;
		}
		let partner = door_partner(cell, cell_state(cur));
		let mut entries = vec![(cell, cur, door_toggled(cur))];
		if let Some(pc) = self.pipeline.cell_at(partner.0, partner.1, partner.2)
			&& BlockType::from_id(cell_id(pc)).is_door()
		{
			entries.push((partner, pc, door_toggled(pc)));
		}
		// A door in a protected cell is furniture, not a mechanism — the
		// same refusal the server makes over its own write list, and
		// scanned over both halves because a leaf can straddle the floor
		// with only its upper cell reachable.
		if entries
			.iter()
			.any(|&(c, _, _)| digblock_core::protect::immutable(c.0, c.1, c.2, &self.protected))
		{
			return;
		}
		// Closing must not trap anyone — locally gate on ourselves; the
		// server gates on every player.
		let closing = digblock_core::chunk::cell_state(cur) & 0b0100 != 0;
		if closing
			&& entries
				.iter()
				.any(|&(c, _, v)| digblock_core::player::placement_blocked(self.body_pos(), c, v))
		{
			return;
		}
		for &(c, _, v) in &entries {
			self.pipeline.set_cell(c.0, c.1, c.2, v);
		}
		self.react(crate::events::GameEvent::DoorToggled { cell });
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		if let Some(net) = self.session.client.as_mut()
			&& net.ready()
		{
			let tick = net.predictor.current_tick();
			let seq = net.edits.begin_multi(&entries, tick, None);
			net.send_edit(
				seq,
				tick,
				origin,
				dir,
				digblock_net::messages::EditKind::Interact,
				hit.cell,
				hit.normal,
				self.hotbar.selected_index() as u8,
			);
		}
	}
}

/// The two fares one sphere drains — its own and the wisp its seal lights — as ONE bill, so the
/// debit, the refund and the award all read the same pair.
fn sphere_fares(level: u32) -> [(digblock_core::block::Essence, u32); 2] {
	[
		digblock_core::alchemy::sphere_price(level),
		digblock_core::alchemy::wisp_price(level),
	]
}
