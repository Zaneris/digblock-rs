//! The fan-out for gameplay moments: every discrete instant worth
//! presenting arrives here as one [`crate::events::GameEvent`], and this
//! match attaches everything it should do — sound today (the
//! `audio::sound_for` table); particles, toasts, camera feel and
//! tutorial advancement hang off the same point later. Call sites fire
//! the event and stay out of the presentation business.

use super::*;

/// One-shot cues a single frame may fire through `react`; the rest drop.
/// Loops and the cadence emitters never pass through here.
const ONESHOTS_PER_FRAME: u8 = 10;
/// Remote-edit cues per frame — a broadcast storm collapses to a few.
const REMOTE_EDIT_CUES_PER_FRAME: u8 = 3;

impl Engine {
	/// React to one gameplay moment. Free when nothing is attached (the
	/// sound path is one branch on a `None` sink).
	pub(super) fn react(&mut self, ev: crate::events::GameEvent) {
		// The tutorial watches everything, AHEAD of the frame budgets —
		// a storm may drop a cue, never a step.
		if let Some(adv) = self.tutorial.observe(&ev) {
			self.on_tutorial_advance(adv);
		}
		if matches!(ev, crate::events::GameEvent::RemoteEdit { .. }) {
			if self.audio.frame_remote_edits >= REMOTE_EDIT_CUES_PER_FRAME {
				return;
			}
			self.audio.frame_remote_edits += 1;
		}
		if self.audio.frame_oneshots >= ONESHOTS_PER_FRAME {
			return;
		}
		self.audio.frame_oneshots += 1;
		if let Some(s) = crate::audio::sound_for(&ev) {
			self.emit(crate::audio::AudioCmd::Sfx {
				sound: s.sfx,
				pos: if s.positional { ev.pos() } else { None },
				gain: s.gain,
			});
		}
		self.spawn_particles(&ev);
		// Camera feel, same table: the dig lands a small kick; an OWN
		// arrival breathes the lens out for a beat.
		match ev {
			crate::events::GameEvent::DigFinished { scooped: false, .. } => {
				self.vfx.camera_fx.kick(0.6);
				self.vfx.camera_fx.add_trauma(0.18);
			}
			crate::events::GameEvent::TeleportArrived { own: true, .. } => {
				self.vfx.camera_fx.pulse_fov(0.08);
			}
			_ => {}
		}
	}

	/// A tutorial step (or a whole chapter) just completed: word it,
	/// sound it, and persist completion through the settings path.
	pub(super) fn on_tutorial_advance(&mut self, adv: crate::tutorial::Advance) {
		match adv {
			crate::tutorial::Advance::Step(step) => {
				self.react(crate::events::GameEvent::TutorialStepDone { step });
				// The siphon exists: the bench closes itself and the reveal
				// cinematic rolls over the nearest live sources.
				if step == crate::tutorial::StepId::CraftSiphon {
					self.start_siphon_cinematic();
				}
			}
			crate::tutorial::Advance::Finished => {
				// Nothing persists: the walkthrough arms only for a 0-XP
				// character, and playing past the dig step is what retires
				// it for this one. The settings row stays a manual switch.
				self.push_feed(crate::voice::Msg::TutorialDone);
				self.react(crate::events::GameEvent::TutorialDone);
			}
			crate::tutorial::Advance::SiphonFinished => {
				// The chapter's own line; the walkthrough setting stays
				// whatever it is — this chapter arms off the nudge instead.
				self.push_feed(crate::voice::Msg::TutorialSiphonDone);
				self.react(crate::events::GameEvent::TutorialDone);
			}
			crate::tutorial::Advance::WispFinished => {
				self.push_feed(crate::voice::Msg::TutorialWispDone);
				self.react(crate::events::GameEvent::TutorialDone);
			}
		}
	}

	/// The siphon reveal shots: a slow arc around the nearest heartwood,
	/// a hard cut to an overhead slide across the nearest springstone,
	/// then the blend home. Candidates come from worldgen but are filmed
	/// only where the LIVE world still agrees (resident, unedited, the
	/// pristine host block standing) — a chopped trunk or a dug-out bed
	/// earns no shot, and with nothing nearby there is no cinematic at
	/// all: the walkthrough just continues on foot.
	fn start_siphon_cinematic(&mut self) {
		use crate::camrig::{CineRig, Cinematic, DroneRig, OrbitRig, Shot};
		if !self.mode.first_person() {
			self.finish_reveal();
			return;
		}
		let mut shots = Vec::new();
		let mut cues = Vec::new();
		let mut t0 = 0.0f32;
		// ~11 degrees of arc pivoting ON the heart itself — the probe
		// walks out of the trunk (`clearance_out_of`), so the block is
		// both the orbit center and what the lens holds. Which heart, and
		// which side of it, comes from `pick_orbit`: the nearest few
		// candidates each offer several arc placements, scored by the
		// clearance the boom will actually get.
		let hearts = self.find_live_nodes(BlockType::Heartwood, 4);
		if let Some((c, yaw, score)) = self.pick_orbit(&hearts) {
			log::info!(
				"siphon cinematic: heartwood at {c:?} (side {yaw:.0} deg, clearance {score:.1})"
			);
			let heart = Vec3::from_array(cell_center(c));
			let mut rig = OrbitRig::new(heart, 6.0, 2.0, 2.5).extended();
			rig.yaw_deg = yaw;
			shots.push(Shot {
				rig: CineRig::Orbit(rig),
				hold: 4.5,
			});
			// The heart appears once the blend has landed and the orbit
			// is framing it.
			cues.push((t0 + 2.2, c));
			t0 += 4.5;
		} else if hearts.is_empty() {
			log::info!("siphon cinematic: no live heartwood in reach");
		} else {
			log::warn!("siphon cinematic: no clear side on any heartwood in reach");
		}
		if let Some(c) = self
			.find_live_nodes(BlockType::Springstone, 1)
			.first()
			.copied()
		{
			log::info!("siphon cinematic: springstone at {c:?}");
			let spring = Vec3::from_array(cell_center(c));
			let d = spring - self.player.pos;
			let toward = Vec3::new(d.x, 0.0, d.z).try_normalize().unwrap_or(Vec3::Z);
			// The drone pass: 15 blocks straight up, gimbal fixed straight
			// down, a slow 10-block slide directly over the bed — the
			// glint drifts through the frame instead of the frame spinning
			// to chase it. The gimbal is yawed so the TRAVEL reads left to
			// right (the spring crosses the frame right to left): a
			// straight-down camera at yaw θ has screen-right
			// (-cos θ, 0, sin θ), and aligning that with the slide
			// direction pins θ to atan2(slide.z, -slide.x). The probe
			// marches out of the bed toward the eye, so the boom holds
			// the full height over open water.
			let slide = toward.cross(Vec3::Y).try_normalize().unwrap_or(Vec3::X);
			let base = spring + Vec3::Y * 15.0;
			shots.push(Shot {
				rig: CineRig::Drone(DroneRig::new(
					base - slide * 5.0,
					base + slide * 5.0,
					spring,
					4.5,
					slide.z.atan2(-slide.x).to_degrees(),
					-89.0,
				)),
				hold: 4.5,
			});
			// The glint surfaces early in the pass, so it lingers.
			cues.push((t0 + 1.8, c));
		} else {
			log::info!("siphon cinematic: no live springstone in reach");
		}
		if shots.is_empty() {
			// Nothing to film: the deferred reveal (if any) sweeps in now.
			self.finish_reveal();
			return;
		}
		// The staged reveal rides the shot clock; without a deferral the
		// world is already showing everything and the cues would be no-ops
		// worth skipping.
		if self.reveal_finale {
			self.reveal_cues = cues;
			self.reveal_clock = 0.0;
		}
		// The bench (or whatever page the craft ran under) closes itself.
		self.ui.inv_screen.close();
		self.ui.alch_screen.close();
		self.ui.char_screen.close();
		self.ui.hiscores.close();
		self.ui.spellbook.close();
		let from = crate::camrig::CamPose {
			pos: self.camera.pos,
			yaw_deg: self.look.yaw_deg,
			pitch_deg: self.look.pitch_deg,
		};
		self.mode = CameraMode::Cinematic(Cinematic::start_shots(from, shots));
	}

	/// The nearest cells that will SHOW `node` once revealed, nearest
	/// first, at most `max`. Worldgen answers where a node WOULD be; the
	/// live checks keep the camera honest about a world the player has
	/// already reshaped.
	fn find_live_nodes(&self, node: BlockType, max: usize) -> Vec<(i32, i32, i32)> {
		use digblock_core::worldgen::nodes;
		let seed = self.seed;
		let (sx, sz) = (
			self.player.pos.x.floor() as i32,
			self.player.pos.z.floor() as i32,
		);
		let live = |x: i32, y: i32, z: i32, host: BlockType| {
			self.pipeline
				.cell_at(x, y, z)
				.is_some_and(|c| digblock_core::chunk::cell_id(c) == host as u16)
				&& !self.pipeline.cell_edited(x, y, z)
		};
		let mut found: Vec<(i64, (i32, i32, i32))> = Vec::new();
		let mut consider = |x: i32, y: i32, z: i32| {
			let (dx, dz) = (i64::from(x - sx), i64::from(z - sz));
			found.push((dx * dx + dz * dz, (x, y, z)));
		};
		// Residency is part of `live`, so the radius only bounds the walk:
		// under a short draw distance the unloaded ring rejects itself.
		// Wide, because hearts ride only ~12% of trees — a near miss with
		// a resident find two chunks further is a cinematic lost for
		// nothing.
		const R: i32 = 96;
		match node {
			BlockType::Heartwood => {
				for z in (sz - R)..(sz + R) {
					for x in (sx - R)..(sx + R) {
						let Some(t) = digblock_core::decoration::trunk_at(x, z, seed) else {
							continue;
						};
						let y = t.y0 + 1;
						if nodes::heartwood_at(x, y, z, seed) && live(x, y, z, BlockType::Wood) {
							consider(x, y, z);
						}
					}
				}
			}
			BlockType::Springstone => {
				for z in (sz - R)..(sz + R) {
					for x in (sx - R)..(sx + R) {
						let Some(y) = nodes::springstone_bed(x, z, seed) else {
							continue;
						};
						if live(x, y, z, BlockType::Sand) {
							consider(x, y, z);
						}
					}
				}
			}
			_ => {}
		}
		found.sort_unstable_by_key(|&(d2, _)| d2);
		found.truncate(max);
		found.into_iter().map(|(_, c)| c).collect()
	}

	/// The best (heart, start azimuth) for the orbit: every candidate
	/// offers eight spokes, ordered outward from the player's side, each
	/// scored by the WORST boom clearance across the planned arc (start,
	/// middle, end — the exact rays `OrbitRig::step` will cast). The
	/// nearest tree with a nearly clear side wins outright; otherwise
	/// the best side seen anywhere; under the floor, no shot at all —
	/// even one tree can have a buried side and an open one, which is
	/// the whole point of asking per spoke.
	fn pick_orbit(&self, hearts: &[(i32, i32, i32)]) -> Option<((i32, i32, i32), f32, f32)> {
		const RADIUS: f32 = 6.0;
		const HEIGHT: f32 = 2.0;
		const ARC_DEG: f32 = 11.25;
		/// Clearance that reads as an unobstructed side (the full ray is
		/// ~6.3; canopy fringes shave a little without hurting the shot).
		const GOOD: f32 = 5.0;
		/// Below this the camera would press against the subject.
		const FLOOR: f32 = 2.5;
		let pipeline = &self.pipeline;
		let cell_at = |x: i32, y: i32, z: i32| pipeline.cell_at(x, y, z);
		let spoke = |heart: Vec3, yaw0: f32| -> f32 {
			let mut worst = f32::MAX;
			for yaw in [yaw0, yaw0 + ARC_DEG * 0.5, yaw0 + ARC_DEG] {
				let (s, c) = yaw.to_radians().sin_cos();
				let offset = Vec3::new(s, 0.0, c) * RADIUS + Vec3::Y * HEIGHT;
				let dist = offset.length();
				let clear = digblock_core::camera_probe::clearance_out_of(
					heart,
					offset / dist,
					dist,
					digblock_core::camera_probe::CAM_RADIUS,
					&cell_at,
				);
				worst = worst.min(clear);
			}
			worst
		};
		let mut best: Option<(f32, (i32, i32, i32), f32)> = None;
		for &c in hearts {
			let heart = Vec3::from_array(cell_center(c));
			let d = self.player.pos - heart;
			let base = d.x.atan2(d.z).to_degrees();
			for k in [0.0, 45.0, -45.0, 90.0, -90.0, 135.0, -135.0, 180.0] {
				let yaw = base + k;
				let score = spoke(heart, yaw);
				if score >= GOOD {
					return Some((c, yaw, score));
				}
				if best.is_none_or(|(b, _, _)| score > b) {
					best = Some((score, c, yaw));
				}
			}
		}
		best.filter(|&(s, _, _)| s >= FLOOR)
			.map(|(s, c, yaw)| (c, yaw, s))
	}

	/// Fire the open/close/select cues from ONE state-delta comparison,
	/// once per frame — covering every toggle path (keys, menu drains,
	/// focus rules, touch) with no scattered hooks, coalesced for free.
	pub(super) fn step_ui_sounds(&mut self) {
		use crate::events::{GameEvent, ScreenKind, UiEvent};
		let now = UiSnapshot {
			menu: self.ui.menu.is_open(),
			inv: self.ui.inv_screen.is_open(),
			alch: self.ui.alch_screen.is_open(),
			character: self.ui.char_screen.is_open(),
			hiscores: self.ui.hiscores.is_open(),
			spellbook: self.ui.spellbook.is_open(),
			chat: self.ui.chat.is_open(),
			hotbar: self.hotbar.selected_index(),
		};
		let Some(prev) = self.audio.ui_snapshot.replace(now) else {
			return;
		};
		let flags = [
			(prev.menu, now.menu, ScreenKind::Menu),
			(prev.inv, now.inv, ScreenKind::Inventory),
			(prev.alch, now.alch, ScreenKind::Alchemy),
			(prev.character, now.character, ScreenKind::Character),
			(prev.hiscores, now.hiscores, ScreenKind::Hiscores),
			(prev.spellbook, now.spellbook, ScreenKind::Spellbook),
			(prev.chat, now.chat, ScreenKind::Chat),
		];
		for (was, is, kind) in flags {
			if is != was {
				self.react(GameEvent::Ui(if is {
					UiEvent::ScreenOpen(kind)
				} else {
					UiEvent::ScreenClose(kind)
				}));
			}
		}
		if now.hotbar != prev.hotbar {
			self.react(GameEvent::Ui(UiEvent::HotbarSelect(now.hotbar as u8)));
		}
	}

	/// The particle half of the fan-out: which burst a moment throws.
	/// Plain digs deliberately throw nothing — the debris chips read as
	/// confetti, not rubble; the water and essence moments keep theirs.
	fn spawn_particles(&mut self, ev: &crate::events::GameEvent) {
		use crate::bursts::BurstKind;
		use crate::events::GameEvent as E;
		let center = |c| Vec3::from_array(cell_center(c));
		match *ev {
			E::DigFinished {
				cell,
				scooped: true,
				..
			} => {
				self.spawn_burst(BurstKind::Splash, center(cell), [0.3, 0.5, 0.8]);
			}
			E::DigFinished {
				cell,
				extracted: true,
				..
			} => {
				// A drained seam releases motes, not rubble.
				self.spawn_burst(BurstKind::Sparkle, center(cell), [1.0, 0.9, 0.5]);
			}
			E::Placed { cell, poured: true } => {
				self.spawn_burst(BurstKind::Splash, center(cell), [0.3, 0.5, 0.8]);
			}
			_ => {}
		}
	}

	/// Spawn one burst at a world point. Over the cap the oldest drops:
	/// a burst that can't draw is better lost than the newest one, which
	/// is the one somebody is watching (confetti's policy).
	pub(super) fn spawn_burst(
		&mut self,
		kind: crate::bursts::BurstKind,
		pos: Vec3,
		color: [f32; 3],
	) {
		self.vfx.burst_seq = self.vfx.burst_seq.wrapping_add(1);
		if self.vfx.bursts.len() >= crate::bursts::MAX_BURSTS {
			self.vfx.bursts.remove(0);
		}
		self.vfx.bursts.push(crate::bursts::instance(
			pos.to_array(),
			0.0,
			// The spread-out seed confetti uses: consecutive bursts must
			// not look like the same throw twice.
			self.vfx.burst_seq as f32 * 7.919,
			kind,
			1.0,
			color,
		));
	}
}
