//! The client half of multiplayer: draining `NetEvent`s, applying
//! authoritative edits and teleports, and the chat feed. `Ack` is
//! dispatched ahead of the edit-budgeted queue — see `pump_net`.

use super::*;
use digblock_net::messages::AdminReply;

impl Engine {
	/// Adopt an authoritative facing: yaw from the horizontal direction,
	/// pitch levelled. Shared by teleports and the spawn facing an admin
	/// set, so the two can never disagree about which way `look` points.
	pub(super) fn face(&mut self, look: Vec3) {
		self.set_look(look.x.atan2(look.z).to_degrees(), 0.0);
	}

	/// The jump's light and sound, shared by our own teleport and a remote's so the two can never
	/// look different: a departure collapse at `from`, an arrival bloom at `to` (both eye
	/// positions). Engine state, so a world swap forgets the bursts like confetti. `own` keys the
	/// halves that belong to the jumper alone — the overlay flash, and an arrival heard flat
	/// rather than placed, because it lands in YOUR ears.
	pub(super) fn jump_bursts(&mut self, from: Vec3, to: Vec3, own: bool) {
		let feet = Vec3::new(0.0, digblock_core::player::EYE_TO_FEET, 0.0);
		let seq = self.vfx.cast_seq;
		self.vfx.cast_seq = seq.wrapping_add(2);
		self.vfx.casts.push(TeleportFx {
			pos: from - feet,
			mode: crate::teleport::Mode::Departure,
			age: 0.0,
			seed: seq as f32 * 17.31,
		});
		self.vfx.casts.push(TeleportFx {
			pos: to - feet,
			mode: crate::teleport::Mode::Arrival,
			age: 0.0,
			seed: (seq + 1) as f32 * 17.31,
		});
		if own {
			self.vfx.flash = 1.0;
		}
		self.react(crate::events::GameEvent::TeleportDeparted {
			from: from.to_array(),
		});
		self.react(crate::events::GameEvent::TeleportArrived {
			to: to.to_array(),
			own,
		});
	}

	/// A sphere SEALED, ours or a remote's: the shell itself was the
	/// spectacle, climbing as ordinary cells through the channel with no
	/// sound of their own (a conjured cell never thunks), so the seal is
	/// the one instant with a voice — the chime, heard flat by the caster
	/// because it closed about THEIR head, and their wash of the overlay.
	pub(super) fn sphere_sealed(&mut self, head: (i32, i32, i32), own: bool) {
		if own {
			self.vfx.flash = 0.6;
		}
		self.react(crate::events::GameEvent::SphereSealed { head, own });
	}

	/// A Sphere's channel BEGAN. The shell's cells never travel: this end draws the same plan from
	/// the same head, radius and block and climbs it over the same span, so one frame stands for
	/// the hundreds of edits a sphere is. EVERY caster's sphere climbs here, ours and a stranger's
	/// alike — one climb per caster — so a remote's shell rises through their channel instead of
	/// appearing whole at the seal. A plan this end cannot draw (or decode) is left to the journal.
	pub(super) fn sphere_winding(
		&mut self,
		by: digblock_net::protocol::PlayerId,
		head: (i32, i32, i32),
		radius: u8,
		span: u32,
		block: u16,
	) {
		if self.sphere_climbs.iter().any(|w| w.by == Some(by)) {
			return;
		}
		let Some((block, writes)) = self.sphere_plan(head, radius, block) else {
			return;
		};
		// No bill: online, the settlement is the server's `InventoryFull`.
		self.sphere_climbs.push(SphereClimb {
			by: Some(by),
			closing: digblock_core::sphere::Closing::new(writes, span),
			head,
			block,
			prefix: 0,
			placed: 0,
			bill: None,
		});
	}

	/// A Sphere's channel ENDED, sealed or unwound — THAT caster's alone. The climb is first walked
	/// to `landed`, the server's count of what it handed the world, since this end's clock is a
	/// round trip out of step either way. Then it answers out of its OWN record — the cells it
	/// wrote and what each replaced — so an unwind puts back exactly what went up; re-deriving
	/// would be the resurrection bug, a plan against a standing sphere naming precisely its holes.
	pub(super) fn sphere_ended(
		&mut self,
		by: digblock_net::protocol::PlayerId,
		head: (i32, i32, i32),
		radius: u8,
		landed: u32,
		sealed: bool,
		block: u16,
	) {
		let own = Some(by) == self.session.my_player_id;
		if let Some(i) = self.sphere_climbs.iter().position(|w| w.by == Some(by)) {
			let mut climb = self.sphere_climbs.remove(i);
			self.land_sphere_to(&mut climb, landed as usize);
			if sealed {
				self.seal_climb(climb, own);
			} else {
				self.unwind_climb(climb);
			}
			return;
		}
		// Nothing climbing here: ours already sealed itself, or this end
		// could not draw the plan when the channel began — a sphere
		// reaching past what we hold — and the sealed one stands up
		// whole now that we can. A broken one left nothing to take down
		// — but our own predicted the seal's light, and the server's
		// word is that there was no seal.
		if own && !sealed {
			self.wisp_left = 0.0;
		}
		if own || !sealed {
			return;
		}
		let Some((_, writes)) = self.sphere_plan(head, radius, block) else {
			return;
		};
		let body = self.body_pos();
		for (c, v) in writes.into_iter().take(landed as usize) {
			if digblock_core::player::placement_blocked(body, c, v) {
				continue;
			}
			self.write_sphere_cell(c, v);
		}
		self.sphere_sealed(head, own);
	}

	/// The sphere as this end draws it — `None` when the world here
	/// cannot answer for every cell, since the plan is all or nothing
	/// (`sphere::plan`), or when the block id is one this build has no
	/// row for. The radius is a server's word, and `sphere::sets` caps
	/// it: the scan is cubic in it.
	fn sphere_plan(
		&self,
		head: digblock_core::sphere::Cell,
		radius: u8,
		block: u16,
	) -> Option<(
		digblock_core::block::BlockType,
		Vec<(digblock_core::sphere::Cell, u16)>,
	)> {
		let block = digblock_core::block::BlockType::try_from_id(block)?;
		let writes = digblock_core::sphere::plan(head, i32::from(radius), block, |c| {
			self.pipeline.cell_at(c.0, c.1, c.2)
		})
		.ok()?;
		Some((block, writes))
	}

	/// Land a teleport (return to spawn, admin /tp): hard-set the player and camera — the
	/// reconciliation slide across half the map would fight streaming — and drop everything that
	/// describes the pre-teleport position. A non-zero `look` adopts that facing (level pitch).
	/// The feed line keys on `spawn_return_pending`, what WE asked for, not on whether a facing
	/// came back: a server with `/set-spawn-dir` set answers a return-to-spawn with a look.
	pub(super) fn apply_teleport(&mut self, pos: Vec3, look: Vec3) {
		let from = self.player.pos;
		self.jump_bursts(from, pos, true);
		self.player.set_state(digblock_core::player::PlayerState {
			pos,
			vel: Vec3::ZERO,
		});
		self.camera.pos = pos;
		self.session.correction_offset = Vec3::ZERO;
		self.session.pending_ack = None;
		self.audio.prev_player_pos = pos;
		self.audio.prev_wet = true;
		self.audio.fall_from_y = pos.y;
		self.session.tick_lerp.reset(pos);
		self.dig = None;
		self.reset_sky_tally();
		if look != Vec3::ZERO {
			self.face(look);
		}
		let line = if std::mem::take(&mut self.save.spawn_return_pending) {
			Msg::ReturnedToSpawn
		} else {
			Msg::Teleported
		};
		self.push_feed(line);
	}

	/// Apply an authoritative block value; the Pipeline holds edits whose
	/// chunk hasn't generated yet and applies them pre-mesh when it lands.
	pub(super) fn apply_remote_edit(&mut self, cell: (i32, i32, i32), cell_value: u16) {
		self.pipeline
			.apply_authoritative(cell.0, cell.1, cell.2, cell_value);
	}

	/// Newest-last message feed; oldest drops when full so a join wave
	/// can't grow the overlay unboundedly.
	pub(super) fn push_feed(&mut self, msg: crate::voice::Msg) {
		self.push_feed_colored(msg, 0xFFFF_FFFF);
	}

	/// Feed line in a specific 0xAABBGGRR color (chat is tinted per
	/// player, matching the sender's body color).
	pub(super) fn push_feed_colored(&mut self, msg: crate::voice::Msg, color: u32) {
		// Every refusal the feed words also blips, from ONE bridge —
		// `feed_sound` is exhaustive over Msg, so a new line decides.
		if let Some(u) = crate::audio::feed_sound(&msg) {
			self.react(crate::events::GameEvent::Ui(u));
		}
		if self.session.feed.len() >= FEED_MAX {
			self.session.feed.remove(0);
		}
		self.session.feed.push((msg, FEED_TTL_SECS, color));
	}

	/// Queue a line rather than showing it now: `update` lets one in every
	/// `FEED_DRIP_SECS`. For a burst that is a LIST — one the player reads
	/// top to bottom — where arriving all at once is arriving as a wall.
	pub(super) fn drip_feed(&mut self, msg: crate::voice::Msg) {
		self.session.feed_queue.push_back((msg, 0xFFFF_FFFF));
	}

	/// Hang up deliberately: say goodbye, then drop the connection. `true` when there was one to
	/// drop. EVERY path that ends a session on purpose goes through here — Leave Server, Leave
	/// World, closing the window. The server frees the account the moment it reads the `Bye`;
	/// skip it and the player waits out an idle timeout before they can rejoin, since one
	/// account may hold only one live connection.
	pub(super) fn leave_server(&mut self) -> bool {
		let Some(mut net) = self.session.client.take() else {
			return false;
		};
		net.disconnect();
		true
	}

	/// Drain the network: remote edits into the pipeline, acks stashed
	/// for the walk branch, disconnects fall back to single-player.
	pub(super) fn pump_net(&mut self, dt: f32) {
		let Some(mut net) = self.session.client.take() else {
			// Single player: no connection to report on. Without this the
			// overlay would keep showing the last session's numbers.
			self.bench.metrics.net = None;
			return;
		};
		net.remotes.advance(dt);
		// Someone else's teleport, read off their snapshot discontinuity —
		// the wire carries no announcement (`RemotePlayers::take_jumps`).
		for (_, from, to) in net.remotes.take_jumps() {
			self.jump_bursts(from, to, false);
		}
		let mut events = std::mem::take(&mut self.session.net_events);
		events.clear();
		net.poll(&mut events, dt);
		use digblock_net::NetEvent;
		// Carry-over first, then this frame's fresh events. Applying edits is bounded per
		// frame, but the ORDER of the whole stream is not negotiable — an Edit followed by a
		// Layout must stay that way — so hitting the cap parks the entire remaining queue
		// rather than letting cheap events overtake the expensive ones. Join sync forces it:
		// the server streams its whole journal as one Edit per cell, and fluid is the same shape.
		let mut queue = std::mem::take(&mut self.session.net_backlog);
		for ev in events.drain(..) {
			match ev {
				// Acks are the one event with NO ordering dependency on
				// edits (they ride datagrams, edits ride the reliable
				// stream — already unordered relative to each other), and
				// they must never wait behind a parked backlog. The
				// predictor's buffer only reaches back `BUF_CAP` ticks:
				// an ack delayed past that falls out of the buffer, so
				// EVERY later snapshot takes the forced-replay path —
				// measured as a continuous stream of replays carrying
				// corrections big enough to see. Join sync on a heavily
				// edited world is enough to get there at
				// NET_EDIT_BUDGET per frame.
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => self.session.pending_ack = Some((acked_input_tick, me)),
				other => queue.push_back(other),
			}
		}
		// Edits are budgeted in TIME (see `NET_EDIT_SHARE`); everything
		// else in the queue is bookkeeping that costs nothing to run.
		// Stopping still parks the WHOLE remaining queue rather than
		// letting cheap events overtake — the ordering rule above is what
		// makes that mandatory, and it is unchanged by the budget's units.
		let edit_deadline = net_edit_budget_ms(self.bench.metrics.refresh_ms());
		let t_edits = web_time::Instant::now();
		let mut edits_applied = 0usize;
		let mut drop_net = false;
		while let Some(next) = queue.front() {
			if matches!(next, NetEvent::Edit { .. }) {
				if edits_applied >= NET_EDIT_MAX {
					break;
				}
				// The floor lands before the clock is ever read, so a
				// frame that is already long still makes progress.
				if edits_applied >= NET_EDIT_MIN
					&& edits_applied.is_multiple_of(NET_EDIT_CHECK)
					&& ms_since(t_edits) >= edit_deadline
				{
					break;
				}
				edits_applied += 1;
			}
			let ev = queue.pop_front().expect("front() just succeeded");
			match ev {
				NetEvent::Welcome {
					seed,
					spawn,
					player_id,
					tod,
					spawn_look,
					..
				} => {
					self.session.my_player_id = Some(player_id);
					// Said once, at the door, and decided by the CLIENT from
					// the master's listing: a server not on the trusted
					// list cannot raise a total, so XP earned here stays
					// here, whatever the server itself might say.
					if self.ui.menu.join_ranked() == Some(false) {
						self.push_feed(crate::voice::Msg::UnrankedServer);
					}
					// NOT `menu.on_welcome()` yet: the Connecting screen IS
					// the join's loading cover, and closing it here is what
					// used to show the player pristine worldgen for the
					// second it took the journal to arrive. It closes when
					// the hold releases.
					self.begin_arrival_hold();
					self.swap_world(seed, spawn);
					// The admin's `/set-spawn-dir`, sent only when this
					// join actually placed us at spawn — a resume in place
					// sends zero and keeps the view we arrived with.
					if spawn_look != Vec3::ZERO {
						self.face(spawn_look);
					}
					// The server owns the world clock. A dev-build --tod
					// freeze still wins: update() re-asserts config.tod
					// every frame.
					self.sun = Sun::with_tod(tod);
				}
				// Unreachable: acks are dispatched above, ahead of the
				// queue. Kept as a backstop so a future event that does
				// get queued still lands rather than being dropped.
				NetEvent::Ack {
					acked_input_tick,
					me,
				} => self.session.pending_ack = Some((acked_input_tick, me)),
				NetEvent::Edit {
					cell,
					cell_value,
					remote_author,
				} => {
					let block = BlockType::from_id(digblock_core::chunk::cell_id(cell_value));
					// Another PLAYER's dig/place gets a quiet cue; fluid
					// results (SERVER_ID) and join sync stay silent.
					if let Some(author) = remote_author
						&& author != digblock_net::protocol::SERVER_ID
					{
						self.react(crate::events::GameEvent::RemoteEdit {
							cell,
							block,
							prev: self.pipeline.block_at(cell.0, cell.1, cell.2),
						});
					}
					self.apply_remote_edit(cell, cell_value);
					// Predictions of ours on this cell are superseded
					// inside `NetClient` — cell to the broadcast, each
					// delta to its own seq's verdict. Refunding here
					// instead is the drift the slab grow made routine: the
					// server accepts the racing second place and debits,
					// while the refund hands the client its item back.
				}
				NetEvent::EditRejected { edit: e, reason } => {
					// Revert the predicted delta always; the CELL only if
					// no authoritative write superseded it — that value
					// owns the cell and must not flick back to `prev`.
					if !e.superseded {
						self.pipeline.set_cell(e.cell.0, e.cell.1, e.cell.2, e.prev);
					}
					self.revert_inv_delta(e.inv_delta);
					// A protected cell is the one rejection a player can
					// act on, and the only one they can't see coming: the
					// world floor is predicted locally, but a spawn lock
					// is server-side knowledge.
					//
					// `inv_delta` is the dig/place discriminator (a
					// positive count is a dig credit, a negative one a
					// place debit) — NOT `placed == Air`, which a slab
					// dig breaks by writing its shrunk value. A
					// multi-cell action carries the delta on its first
					// entry only, so this speaks once per action rather
					// than once per cell.
					if reason == digblock_net::messages::RejectReason::Protected {
						match e.inv_delta {
							Some((_, n)) if n > 0 => self.push_feed(Msg::DigProtected),
							Some((_, n)) if n < 0 => self.push_feed(Msg::PlaceProtected),
							_ => {}
						}
					}
				}
				NetEvent::DigRefused { cell, reason } => {
					// Abandon the hold the moment the server says no, so
					// the progress bar stops instead of running the full
					// dig time out against a block that can't break.
					// Recording the cell — for EVERY reason, not just the
					// ones we can name — is what stops `step_dig` opening
					// a fresh hold and re-sending DigStart each frame (a
					// reason-gated latch silently spun on any refusal this
					// build didn't know).
					if self.dig.as_ref().is_some_and(|d| d.cell == cell) {
						self.cancel_dig();
					}
					if self.refused_cell != Some(cell) {
						self.refused_cell = Some(cell);
						use digblock_core::unlock::{self, Kind};
						use digblock_net::messages::RejectReason as R;
						// The wire carries a REASON and never a sentence, so the message is
						// built here, over the block still under the crosshair, so a verdict
						// speaks exactly what prediction would have. A cell this client can no
						// longer see is the one case with nothing of its own to say.
						let msg = {
							let named = |kind| {
								self.pipeline
									.block_at(cell.0, cell.1, cell.2)
									.and_then(|b| unlock::requirement(b, kind))
									.map(Msg::DigRefused)
							};
							match reason {
								R::Protected => Some(Msg::DigProtected),
								R::SkillGated => named(Kind::Strength),
								R::NotUnlocked => named(Kind::Recipe),
								R::NeedsTool => Some(Msg::NeedsSiphon),
								_ => None,
							}
						};
						self.push_feed(msg.unwrap_or(Msg::DigRefusedByServer));
					}
				}
				NetEvent::Inventory { counts, pending } => {
					// Authoritative base (strict wire decode, sub-material
					// families included) + the predictions that were still in
					// flight WHEN THE SERVER'S COUNTS WERE DECODED. Reading
					// the live pending set here instead would be a different
					// moment — see `NetEvent::Inventory`.
					// The join sync's first counts are state arriving, not a
					// craft happening — remembered here so the reveal below
					// re-meshes silently for a siphon you already had.
					let arriving = self.auth_essence.is_none();
					let mut inv = Inventory::from_wire(&counts);
					self.adopt_auth_essence(&inv);
					for (b, n) in pending {
						match n {
							n if n > 0 => inv.add(b, n as u32),
							n if n < 0 => {
								let _ = inv.try_take(b, (-n) as u32);
							}
							_ => {}
						}
					}
					self.inventory = inv;
					self.nudge_siphon();
					// The pack is what reveals the surface nodes: a siphon in
					// these counts for the first time (the server's echo of the
					// craft) is the moment they fade into the world.
					self.sync_node_reveal(!arriving);
					// New types file bar-first then grid; stale grid slots
					// clear — same derivation the server runs.
					digblock_core::inv_grid::reconcile_layout(
						&self.inventory,
						&mut self.hotbar,
						&mut self.inv_grid,
					);
				}
				NetEvent::Layout { hotbar, grid } => {
					// The identity's stored arrangement (join sync).
					// Sanitizing decode, keep the live selection, then
					// reconcile against the counts.
					use digblock_core::hotbar::{HOTBAR_SLOTS, WIRE_EMPTY};
					let mut bar_wire = [WIRE_EMPTY; HOTBAR_SLOTS];
					for (d, s) in bar_wire.iter_mut().zip(hotbar.iter()) {
						*d = *s;
					}
					let selected = self.hotbar.selected_index();
					self.hotbar = Hotbar::from_wire(&bar_wire);
					self.hotbar.select_digit(selected as u8 + 1);
					self.inv_grid = digblock_core::inv_grid::InvGrid::from_wire(&grid);
					// Prime the dedup with what the server holds (its
					// sanitized copy); the reconcile below normally lands on
					// exactly this, and any divergence gets reported by the
					// per-frame layout sync.
					self.session.last_layout_sent = Some((
						self.hotbar.to_wire().to_vec(),
						self.inv_grid.to_wire().to_vec(),
					));
					digblock_core::inv_grid::reconcile_layout(
						&self.inventory,
						&mut self.hotbar,
						&mut self.inv_grid,
					);
				}
				NetEvent::SpellBar { slots } => {
					// Join sync: adopt the stored assignments (validated on
					// read — unknown ids fall empty) and prime the dedup so
					// the echo does not resend.
					self.spell_bar = crate::spellbar::SpellBar::from_wire(&slots);
					self.session.sent_spell_bar = Some(self.spell_bar.to_wire());
				}
				NetEvent::Skills { xp } => {
					// Join sync: adopt wholesale, no toasts — this is
					// state arriving, not an award happening. The ore
					// reveal still has to follow it, or a Mining-60 player
					// joins a server and sees plain stone until the next
					// level-up.
					self.skills = digblock_core::skills::SkillSet::from_wire(&xp);
					self.sync_node_reveal(false);
					// A character with no light of their own enters their
					// held evening; the join's InventoryFull landed just
					// ahead of this frame, and the server's clock keeps
					// running underneath, tracked from the sun the Welcome
					// anchored.
					self.arm_dusk_hold(Some(self.sun.tod));
				}
				NetEvent::XpGain { skill, total } => {
					if let Some(s) = digblock_core::skills::Skill::try_from_index(skill) {
						self.apply_xp_total(s, total);
					}
				}
				NetEvent::LevelUp {
					player,
					skill,
					level,
				} => {
					// Someone else levelled: confetti over THEIR head. The
					// position is last frame's interpolated sample — the
					// same one their box was drawn at, which is the only
					// place the paper could honestly come from. A player
					// too far away to be sampled at all gets no burst,
					// which is exactly right: nobody can see them.
					if let Some(d) = self.vfx.remote_scratch.iter().find(|d| d.id == player) {
						let eye = d.pos;
						self.throw_confetti(eye);
					}
					if let Some(s) = digblock_core::skills::Skill::try_from_index(skill) {
						let who = self
							.session
							.player_names
							.get(&player)
							.cloned()
							.unwrap_or_else(|| crate::voice::unknown_player(player));
						self.push_feed(Msg::RemoteLevelUp {
							who,
							skill: s,
							level,
						});
					}
				}
				NetEvent::PlayerJoined { id, name } => {
					self.push_feed(Msg::Joined { name: name.clone() });
					self.session.player_names.insert(id, name);
				}
				NetEvent::PlayerLeft { id } => {
					let name = self
						.session
						.player_names
						.remove(&id)
						.unwrap_or_else(|| crate::voice::unknown_player(id));
					self.push_feed(Msg::Left { name });
				}
				NetEvent::Teleported { pos, look } => {
					self.apply_teleport(pos, look);
					// A teleport is an arrival too — the destination's
					// journal is no more synced than a join's.
					self.begin_arrival_hold();
				}
				NetEvent::AreaReady => self.end_arrival_hold(),
				NetEvent::Waystones { entries } => {
					// The server's resolved sites: the spellbook's roster
					// and the standing spirals' anchors. Marker sites are
					// pad-floor positions; entries carry eyes.
					self.session.waystones = entries;
					self.refresh_marker_sites();
				}
				NetEvent::Kindled { cell, secs_left } => {
					self.kindle_light((cell[0], cell[1], cell[2]), f32::from(secs_left));
				}
				NetEvent::SphereWinding {
					by,
					head,
					radius,
					span,
					block,
				} => self.sphere_winding(by, (head[0], head[1], head[2]), radius, span, block),
				NetEvent::SphereEnded {
					by,
					head,
					radius,
					landed,
					sealed,
					block,
				} => self.sphere_ended(
					by,
					(head[0], head[1], head[2]),
					radius,
					landed,
					sealed,
					block,
				),
				NetEvent::CastRefused { reason } => {
					use digblock_net::messages::CastRefusal;
					// A predicted wisp goes out with the refusal — the
					// debit it predicted against never happened. A Sphere
					// channel still winding has nothing climbing under
					// it, so it stops here rather than completing into
					// nothing.
					self.wisp_left = 0.0;
					// And a predicted road — but only if THIS refusal is
					// the one answering it. Its planks were predicted
					// edits, so the standing rollback takes each one back
					// as its own rejection arrives; ending here stops more
					// being laid against a debit that never happened.
					if self.last_cast == Some(CastTarget::Causeway) {
						self.end_causeway();
					}
					if self.cast.as_ref().map(|c| c.target) == Some(CastTarget::Sphere) {
						self.cast = None;
						self.emit(crate::audio::AudioCmd::LoopStop {
							id: crate::audio::LOOP_CAST,
						});
					}
					// The three block reasons name the Sphere on their own —
					// no other spell takes a block — but not what was held
					// or how short: the honest gates in `begin_sphere` say
					// that, and this is the race they lost.
					self.push_feed(match reason {
						CastRefusal::Locked => Msg::WaystoneLocked,
						CastRefusal::CantPay => Msg::CastCantPay,
						CastRefusal::BadTarget => Msg::CastBadTarget,
						CastRefusal::NoBlock => Msg::SphereNoBlock,
						CastRefusal::BadBlock => Msg::SphereBadBlock(None),
						CastRefusal::BlocksShort => Msg::SphereBlocksShort(None),
					});
				}
				// Hard-set: inter-beat drift is microscopic, and after a
				// paused clock (backgrounded tab) a snap is what we want.
				// An active held dusk consumes the beat instead — it is
				// what tracks the server's clock while it paints its own.
				NetEvent::TimeOfDay { tod } => {
					if !self.dusk_hold.on_server_tod(tod) {
						self.sun.tod = tod;
					}
				}
				NetEvent::AdminCommands { commands } => self.ui.chat.set_commands(commands),
				// The catalog is a list and arrives as a dozen replies in
				// one drain, so it is spoken a couple a second. Every other
				// reply answers something just typed and waits for nothing.
				NetEvent::AdminReply(
					reply @ (AdminReply::CommandHint { .. } | AdminReply::CompassHint),
				) => self.drip_feed(Msg::Admin(reply)),
				NetEvent::AdminReply(reply) => self.push_feed(Msg::Admin(reply)),
				NetEvent::Chat { from, text } => {
					if from == digblock_net::protocol::SERVER_ID {
						// Server-authored free-form line (admin replies are
						// typed and arrive as `AdminReply` above):
						// bracketed, neutral white — not a player voice.
						self.push_feed(Msg::ServerNotice { text });
						continue;
					}
					// Our own echo: the roster never carries ourselves, so
					// resolve through the session name instead.
					let name = if Some(from) == self.session.my_player_id {
						self.config
							.player_name
							.clone()
							.unwrap_or_else(crate::voice::me)
					} else {
						self.session
							.player_names
							.get(&from)
							.cloned()
							.unwrap_or_else(|| crate::voice::unknown_player(from))
					};
					let c = player_color(from);
					self.push_feed_colored(Msg::Chat { name, text }, pack_rgb(c));
				}
				NetEvent::Refused { reason } => {
					self.push_feed(Msg::ConnectRefused {
						reason: reason.clone(),
					});
					log::warn!("server refused: {reason}; continuing single-player");
					self.ui.menu.on_net_failed(&reason);
					drop_net = true;
				}
				NetEvent::Disconnected { reason } => {
					self.push_feed(Msg::Disconnected {
						reason: reason.clone(),
					});
					log::warn!("disconnected: {reason}; continuing single-player");
					self.ui.menu.on_net_failed(&reason);
					drop_net = true;
				}
			}
		}
		self.session.net_events = events;
		// A disconnect voids everything still queued; keeping it would
		// apply a dead session's edits to whatever world comes next.
		if drop_net {
			queue.clear();
		}
		self.bench.metrics.gauges.net_backlog = queue.len() as u32;
		self.session.net_backlog = queue;

		// Cleared on the way out, so the overlay reads "offline" the moment
		// a session ends rather than freezing on its last numbers.
		self.bench.metrics.net = (!drop_net).then(|| net.stats());
		if !drop_net {
			self.session.client = Some(net);
		} else {
			self.session.pending_ack = None;
			self.session.correction_offset = Vec3::ZERO;
			self.session.my_player_id = None;
			self.session.last_layout_sent = None;
			self.session.sent_spell_bar = None;
			// No one left to talk to: discard any half-typed chat line.
			if self.ui.chat.is_open() {
				self.ui.chat.handle_input(&InputEvent::Key {
					key: Key::Escape,
					pressed: true,
				});
			}
		}
	}
}
