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

	/// The owned tool roster in deterministic block-id order: the
	/// always-owned bucket plus every tool id with a nonzero count.
	///
	/// The predicate lives in core so the rail this feeds and the
	/// ownership check `reconcile_layout` makes are literally the same
	/// question — a client offering a tool its own server would take back
	/// is a rejected swing, not a feature.
	pub(super) fn owned_tools(&self) -> Vec<BlockType> {
		self.inventory.owned_tools().collect()
	}

	/// True while the tool is out AND empty — the scooping state. This is
	/// the one that changes the picking ray, because it is the only state
	/// in which the thing you want to hit is a fluid source. Charged, the
	/// bucket aims at ordinary blocks like every other item, so it picks
	/// with the ordinary ray and the outline lands on the face you're
	/// about to pour against.
	pub(super) fn scoop_ready(&self) -> bool {
		self.water_tool_selected() && !self.inventory.bucket_full()
	}

	/// Where the collidable body actually is, for occupancy questions
	/// ("would this placement trap me?"). Deliberately NOT `camera.pos`:
	/// the camera carries the tick interpolation and the decaying
	/// reconciliation offset, so it can sit up to `POS_TOLERANCE` from the
	/// body — and the server answers the same question from its own
	/// simulated position, so a camera-based answer mispredicts. Aim rays
	/// still come from the camera: you hit what you see.
	pub(super) fn body_pos(&self) -> Vec3 {
		if self.input.walking {
			self.player.pos
		} else {
			self.camera.pos // fly leaves player.pos stale
		}
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
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		let pipeline = &self.pipeline;
		let cell_at = |x: i32, y: i32, z: i32| pipeline.cell_at(x, y, z);
		self.target = if self.scoop_ready() {
			raycast_fluid(origin, dir, REACH, &cell_at)
		} else {
			raycast(origin, dir, REACH, &cell_at)
		};
	}

	/// Is `hit` something the current tool can dig?
	/// The local world's spawn: the same edit-aware resolve the server
	/// runs, against this client's own edit overlay.
	///
	/// A world with no overlay (an unsaved session) has no recorded edits
	/// to reason about, so the anchor stands; `ascend_clear` still steps
	/// over anything built there this session.
	/// The offline spawn as resolved against the save's edits — the eye
	/// the arrival lock centres on. Split from [`Self::local_spawn`]
	/// because the lock must anchor on the PAD, before the landing rule
	/// steps over any residual obstruction.
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

	/// What a dig on this cell IS for us right now (SKILLS-DESIGN §5.3)
	/// — the same `ore::dig_mode` the server asks of its own world, so
	/// prediction and authority cannot disagree about whether an action
	/// was a dig, a draw, or a refusal.
	///
	/// The chunk store still holds Stone here: the reveal is a stamp on
	/// the MESH input, never a world write, which is what keeps ore out of
	/// the journal and out of everyone else's world.
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
			digblock_core::worldgen::nodes::Levels::of(&self.skills),
			self.pipeline.cell_edited(cell.0, cell.1, cell.2),
			self.hotbar.tool_in_hand(),
		)
	}

	/// Has this cell ever been written — the provenance half of
	/// `BlockType::dig_credit`, and the same bit `ore_dig_mode` above
	/// passes to the reveal. Read off the `EditOverlay`, which MIRRORS the
	/// server's journal in multiplayer, so both ends answer alike.
	///
	/// A place of one's own is recorded when the server echoes it rather
	/// than when it is predicted (recording predictions is unfixable — a
	/// rejection reverts through `set_cell` and `record` is insert-only),
	/// so for one round trip a stone you just laid still reads as the
	/// world's. That window cannot be reached by hand: a Rock-family hold
	/// is orders of magnitude longer than a round trip, and the pick only
	/// lengthens it.
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
		let id = digblock_core::chunk::cell_id(hit.cell_value);
		let credit = hit.block.dig_credit(self.cell_edited(hit.cell), alchemy);
		let mut out = digblock_core::pick::cells(
			hit.cell,
			hit.normal,
			self.camera.dir().to_array(),
			digblock_core::unlock::pick_cells(level),
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
			hit.block.props().collides && hit.block.dig_hardness(hit.cell.1).is_some()
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

	/// Begin a cast channel. The home spell on cooldown opens the
	/// spellbook instead — the countdown is visible there, and a silent
	/// no-op reads as a broken key.
	pub(super) fn start_cast(&mut self, target: CastTarget) {
		if self.cast.is_some() {
			return;
		}
		if target == CastTarget::Home && self.save.spawn_cooldown > 0.0 {
			self.input.keys = HeldKeys::default();
			self.input.dig_held = false;
			self.ui.spellbook.open();
			return;
		}
		self.cast = Some(CastState { target, ticks: 0 });
		self.emit(crate::audio::AudioCmd::LoopStart {
			id: crate::audio::LOOP_CAST,
			sound: crate::audio::Sfx::CastLoop,
			pos: None,
			gain: 0.45,
		});
	}

	/// One fixed-rate step of the cast channel. A channel is STILLNESS:
	/// any movement intent, a dig hold, or a surface opening resets it
	/// outright — the decay mold is a dig's, deliberately not borrowed.
	pub(super) fn step_cast(&mut self) {
		if self.cast.is_none() {
			return;
		}
		let keys = &self.input.keys;
		let moving = keys.w
			|| keys.a || keys.s
			|| keys.d || keys.up
			|| keys.down
			|| self.input.touch.move_axis() != glam::Vec2::ZERO;
		let surface = self.ui.menu.is_open()
			|| self.ui.inv_screen.is_open()
			|| self.ui.alch_screen.is_open()
			|| self.ui.char_screen.is_open()
			|| self.ui.spellbook.is_open()
			|| self.ui.chat.is_open();
		if moving || surface || self.dig_active() {
			self.cast = None;
			self.emit(crate::audio::AudioCmd::LoopStop {
				id: crate::audio::LOOP_CAST,
			});
			self.push_feed(Msg::CastInterrupted);
			return;
		}
		let need = digblock_core::unlock::channel_ticks(
			self.skills.level(digblock_core::skills::Skill::Arcana),
		);
		let done = {
			let cast = self.cast.as_mut().expect("checked above");
			cast.ticks += 1;
			(cast.ticks >= need).then_some(cast.target)
		};
		if let Some(target) = done {
			self.cast = None;
			self.emit(crate::audio::AudioCmd::LoopStop {
				id: crate::audio::LOOP_CAST,
			});
			self.complete_cast(target);
		}
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

	/// The offline arm of a waystone cast: the same tables the server
	/// validates with, the same debit, and the site materializes on first
	/// visit — stamped, protected and marked, what a server world did at
	/// boot. The cast pays the roster's walk up to its own rung (pure
	/// ring searches; the ladder resolves in order because the separation
	/// rule is mutual), kept for the session. No first-arrival milestone
	/// offline: that award is once per ACCOUNT, and accounts are a
	/// multiplayer concept — a per-save copy would re-pay every save.
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
			self.push_feed(Msg::WaystoneLocked);
			return;
		}
		let Some(eye) = self.save.offline_waystones.site(idx) else {
			// This seed never made such a place — a port to nowhere.
			self.push_feed(Msg::WaystoneLocked);
			return;
		};
		let (essence, fare) =
			digblock_core::alchemy::cast_price(kind).expect("a laddered kind has a fare");
		if !self.inventory.try_take_sub(essence, fare) {
			self.push_feed(Msg::CastCantPay);
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
		self.award_xp_offline(Skill::Arcana, digblock_core::skills::cast_xp(fare));
	}

	/// A completed channel fires its spell. The teleport that answers —
	/// this frame offline, a round trip later online — is what words the
	/// arrival, via `spawn_return_pending`.
	fn complete_cast(&mut self, target: CastTarget) {
		use digblock_core::skills::Skill;
		match target {
			CastTarget::Home => {
				self.save.spawn_cooldown =
					digblock_core::unlock::return_cooldown_secs(self.skills.level(Skill::Arcana))
						as f32;
				self.save.spawn_return_pending = true;
				match self.session.client.as_mut() {
					Some(net) if net.ready() => net.send_return_to_spawn(),
					_ => {
						// Same rule the server runs: relocate sideways out
						// of anything dug into the spawn, never descend
						// into it. The overlay — not `pipeline.cell_at` —
						// is the edit view: the pipeline answers None for
						// unloaded columns, which the support test would
						// read as void.
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
			CastTarget::Waystone(idx) => match self.session.client.as_mut() {
				// The server validates the two real gates — the unlock
				// and the fare — and answers Teleported or CastRefused.
				Some(net) if net.ready() => {
					net.send_cast(digblock_net::messages::CastKind::Waystone(idx));
				}
				_ => self.cast_waystone_offline(idx),
			},
			CastTarget::Wisp => {
				let (essence, fare) = digblock_core::alchemy::wisp_price();
				match self.session.client.as_mut() {
					Some(net) if net.ready() => {
						// Predicted: the light shows now, the debit rides
						// the rebase, and a CastRefused puts it out.
						net.send_cast(digblock_net::messages::CastKind::Wisp);
						self.wisp_left =
							digblock_core::unlock::wisp_secs(self.skills.level(Skill::Arcana))
								as f32;
					}
					_ => {
						if !self.inventory.try_take_sub(essence, fare) {
							self.push_feed(Msg::CastCantPay);
							return;
						}
						self.wisp_left =
							digblock_core::unlock::wisp_secs(self.skills.level(Skill::Arcana))
								as f32;
						self.award_xp_offline(Skill::Arcana, digblock_core::skills::cast_xp(fare));
					}
				}
			}
			CastTarget::Kindle => {
				// The air cell just off the aimed face — resolved at
				// COMPLETION, so the caster aims through the channel.
				let Some(hit) = self.target else {
					self.push_feed(Msg::CastBadTarget);
					return;
				};
				let cell = (
					hit.cell.0 + hit.normal.0,
					hit.cell.1 + hit.normal.1,
					hit.cell.2 + hit.normal.2,
				);
				match self.session.client.as_mut() {
					Some(net) if net.ready() => {
						net.send_cast(digblock_net::messages::CastKind::Kindle {
							cell: [cell.0, cell.1, cell.2],
							face: 0,
						});
					}
					_ => self.cast_kindle_offline(cell),
				}
			}
		}
	}

	/// The offline arm of conjured light: the same rung, target and fare
	/// rules the server runs, applied through the pipeline. The wisplight
	/// sweep both arms register is the whole expiry.
	fn cast_kindle_offline(&mut self, cell: (i32, i32, i32)) {
		use digblock_core::skills::Skill;
		let level = self.skills.level(Skill::Arcana);
		if level < digblock_core::unlock::KINDLE_AT {
			self.push_feed(Msg::WaystoneLocked);
			return;
		}
		if digblock_core::protect::immutable(cell.0, cell.1, cell.2, &self.protected)
			|| self.pipeline.cell_at(cell.0, cell.1, cell.2)
				!= Some(digblock_core::block::BlockType::Air as u16)
		{
			self.push_feed(Msg::CastBadTarget);
			return;
		}
		let (essence, fare) = digblock_core::alchemy::kindle_price(level);
		if !self.inventory.try_take_sub(essence, fare) {
			self.push_feed(Msg::CastCantPay);
			return;
		}
		self.pipeline.set_cell(
			cell.0,
			cell.1,
			cell.2,
			digblock_core::block::BlockType::Wisplight as u16,
		);
		self.award_xp_offline(Skill::Arcana, digblock_core::skills::cast_xp(fare));
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
			// A resume keeps its banked progress; anything else drops what
			// it was holding (and tells the server, which is what stops it
			// verifying aim on a cell we walked away from). Either way the
			// hold re-opens from here, DigStart included — the server was
			// told to stop the moment the button came up, so a resume has
			// to ask it to start counting again.
			let carried = match same {
				Some((_, banked)) => banked,
				None => {
					self.cancel_dig();
					0
				}
			};
			// Family-scaled hold time (SIM v12) — the same `dig_required`
			// the server's completion gate runs.
			// An EXTRACTION is timed as the ORE rather than as the stone
			// the cell still holds, so a deep seam takes longer than a
			// shallow one and the ore rows' hardnesses are not dead
			// numbers on blocks nobody digs.
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
				self.emit(crate::audio::AudioCmd::Sfx {
					sound: crate::audio::Sfx::Swim,
					pos: Some(cell_center(hit.cell)),
					gain: 0.7,
				});
			} else {
				// A draw rides the SAME voice as a dig, so every stop path
				// — release, cancel, completion, world swap — already
				// covers it, and the pitch ramp below reads as a resonance
				// winding up rather than as a pick speeding up. WHICH sound
				// it is belongs to `audio::dig_voice`, together with the
				// finish it has to agree with.
				let extracting =
					matches!(mode, digblock_core::worldgen::nodes::DigMode::Extract(_));
				self.emit(crate::audio::AudioCmd::LoopStart {
					id: crate::audio::LOOP_DIG,
					sound: crate::audio::dig_voice(extracting, hit.block).0,
					pos: Some(cell_center(hit.cell)),
					// Under the dig loop: a sustained hum at a hammer's
					// level would wear out over a hold that runs longer
					// than any dig.
					gain: if extracting { 0.85 } else { 1.0 },
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

	/// Stop digging, but KEEP what the hold banked: progress bleeds back to
	/// untouched over `skills::DIG_DECAY_TICKS` and a hold resumed inside that
	/// window inherits what is left, so a thumb slipping off the button no
	/// longer costs the whole block. Called once per sim step while not digging
	/// — that repetition is what advances the decay, and dropping the record at
	/// zero is what keeps a spent hold from being resumed for free.
	///
	/// The server is told once, on the transition, and runs the identical decay
	/// from ITS release to ITS resume. Those two windows are the same interval
	/// on a stable link, so only the JITTER separates them, which is what
	/// `dig_grace` already exists to absorb.
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
		// (cell, prev) list this dig clears — a door adds its partner, the
		// Resonant Pick adds the plane of the face (SKILLS-DESIGN §5.6).
		// Every extra cell holds the SAME id and is the plain breakable
		// case (`pick::breaks_as_extra`), which is what makes
		// `begin_multi`'s single `inv_delta` a correct description of the
		// whole group — and what keeps the aimed door's own partner out of
		// the plane, where it used to be counted AND written twice.
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
		// A filled bucket splashes; everything else ends the way its hold
		// began, through the pair `audio::dig_voice` owns — a drawn seam
		// lets go, a broken block breaks. Water's material class is Earth
		// (the table's total fallback), which is right for nothing it does,
		// which is why it is the one case answered here.
		let tool = self.water_tool_selected();
		let extracted = extracting.is_some();
		let (end, end_gain) = if tool {
			(crate::audio::Sfx::Splash, 0.5)
		} else {
			(
				crate::audio::dig_voice(extracted, prev).1,
				if extracted { 0.9 } else { 1.0 },
			)
		};
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: end,
			pos: Some(cell_center(hit.cell)),
			gain: end_gain,
		});
		// No swing is started here: the arc that began with this hold is running
		// out on this very frame, and re-striking it bolts a whole second
		// stroke onto a finished scoop. One credit even for the two-cell door.
		//
		// An extraction credits NOTHING here, and that is not an omission. A
		// drawn seam pays essence (`alchemy::field_yield`) rather than a block,
		// and essence arrives as a SPLIT, which a `PendingEdit`'s single
		// `inv_delta` cannot carry. So online it is deliberately un-predicted
		// and the server's `InventoryFull` rebase is the answer, the same path
		// `ClientMsg::Alchemy` takes; offline the arm below pays it, where the
		// engine is the authority. The CELL is predicted on both, so the vein
		// turns spent under the player's hand with no latency either way.
		//
		// Everything else pays the `credit` resolved above the write: a stone
		// somebody MADE comes back whole where the mountain's crumbles.
		let drop = if extracting.is_some() { None } else { credit };
		if let Some(d) = drop {
			self.inventory.add(d, broken);
			self.hotbar.auto_assign(d);
		}
		let (origin, dir) = (self.camera.pos, self.camera.dir());
		match self.session.client.as_mut() {
			Some(net) if net.ready() => {
				// `tool` is `water_tool_selected` — the SLOT is not enough.
				// It is the same predicate `diggable` and the scoop ray use,
				// and the same one the server's Collect gate asks
				// (`slot_item == WaterTool && active_tool == Bucket`).
				// Reading the slot alone called every hold with the siphon,
				// pick or sigil in hand a Collect, which the server answered
				// WrongSlot: cell and credit reverted, so with any tool but
				// the bucket active the player could dig NOTHING online —
				// veins included — while offline the identical input worked.
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
						ore, alchemy, mining, hit.cell, self.seed,
					) {
						self.inventory.add_sub(e, n);
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
			// The charged bucket pours its one source. It works here, and
			// only here, because a charged bucket picks with the ORDINARY
			// ray: the fluid ray stops INSIDE a source, leaving no face
			// normal to build against, which is why right-clicking water
			// with the old collect-only tool did nothing at all.
			//
			// The BUCKET, not the slot: the server's place gate requires
			// `active_tool == Bucket`, so pouring with the pick or siphon
			// active answered WrongSlot and the source appeared and
			// vanished. Offline there was no such correction and it simply
			// poured — a pick that puts down water.
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
		// A slab onto a same-material slab's OPEN face — the top of a
		// floor slab, the underside of a ceiling one — grows the hit cell
		// in place (1/3 → 2/3 → base block) instead of writing the
		// neighbor. The wire still carries the canonical state-0 slab —
		// the server re-derives both the grow and the anchor from its own
		// raycast.
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
		// The Mason's Arc: with the stance on and the rank earned, a
		// plain block extends into the full line along the face normal —
		// ALL OR NOTHING (both ends must derive one set), falling back to
		// the ordinary single place when the line does not fit. Slabs,
		// doors and water keep their own machinery, un-multiplied.
		let mut arc_len = 1u32;
		if self.arc_on && !grow && !block.is_door() && !block.is_slab() && block != BlockType::Water
		{
			let n = digblock_core::unlock::arc_cells(
				self.skills.level(digblock_core::skills::Skill::Arcana),
			);
			if n > 1 {
				let mut line = Vec::new();
				for i in 2..=n as i32 {
					let c = (
						hit.cell.0 + hit.normal.0 * i,
						hit.cell.1 + hit.normal.1 * i,
						hit.cell.2 + hit.normal.2 * i,
					);
					if !self.cell_is_replaceable(c)
						|| digblock_core::protect::immutable(c.0, c.1, c.2, &self.protected)
						|| digblock_core::player::placement_blocked(self.body_pos(), c, placed_cell)
					{
						line.clear();
						break;
					}
					line.push((c, placed_cell));
				}
				if line.len() == n - 1 {
					arc_len = n as u32;
					placements.extend(line);
				}
			}
		}
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
		// One debit covers a door's two cells; an arc costs its length.
		if !self.inventory.try_take(block, arc_len) {
			return;
		}
		let mut entries = Vec::new();
		for &(c, v) in &placements {
			match self.pipeline.set_cell(c.0, c.1, c.2, v) {
				Some(prev) => entries.push((c, prev, v)),
				None if entries.is_empty() => {
					self.inventory.add(block, arc_len); // refund: nothing happened
					return;
				}
				None => {}
			}
		}
		// A poured source splashes; everything else lands with a thunk.
		let poured = block == BlockType::Water;
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: if poured {
				crate::audio::Sfx::Splash
			} else {
				crate::audio::Sfx::Place
			},
			pos: Some(cell_center(target)),
			gain: if poured { 0.6 } else { 0.9 },
		});
		match self.session.client.as_mut() {
			Some(net) if net.ready() => {
				let tick = net.predictor.current_tick();
				let seq = net
					.edits
					.begin_multi(&entries, tick, Some((block, -(arc_len as i32))));
				// Slabs always travel as the canonical state-0 cell: the
				// server derives the grow AND the anchor from the normal
				// it verified, so neither is a state a client can assert.
				let wire_cell = if grow || block.is_slab() {
					block as u16
				} else {
					placed_cell
				};
				let kind = if arc_len > 1 {
					digblock_net::messages::EditKind::PlaceArc { block: wire_cell }
				} else {
					digblock_net::messages::EditKind::Place { block: wire_cell }
				};
				net.send_edit(
					seq,
					tick,
					origin,
					dir,
					kind,
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
		self.emit(crate::audio::AudioCmd::Sfx {
			sound: crate::audio::Sfx::Place,
			pos: Some(cell_center(cell)),
			gain: 0.6,
		});
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
