//! Carried lights: the per-frame list of emitters that hang on a body rather
//! than in a cell (GI-DESIGN.md R28).
//!
//! **A carried light is an ordinary entry in the same light list a placed torch
//! uses** — there is no second lighting path, which is what gives one bounce,
//! hard shadows, water response and a lit hand for free. It hangs at
//! `gi::carried_origin`, a function of POSITION and not facing, so the cell is
//! provably non-solid and mouse-look cannot move it.
//!
//! Motion is expressed as RE-PLACEMENT — travel `CARRY_INVAL_STEP` and it fires
//! the same `gi_invalidate` a block edit fires. The trigger is a DISTANCE, never
//! a cell crossing, which on a boundary would fire every frame for a player
//! going nowhere. Putting a light OUT re-places it too, or the probes hold its
//! bounce for seconds after the list has dropped it.
//!
//! Fires are banked by [`Carried::commit`] only once the renderer has actually
//! queued them.

use super::{held_icon, player_color};
use digblock_core::BlockType;
use glam::Vec3;

/// Carrier key for the local player. Remote carriers key on their
/// `PlayerId`, which is a `u8`, so the two spaces cannot collide.
pub(super) const CARRIER_SELF: u16 = u16::MAX;
/// Carrier key for the local player's wisp — a SECOND light on the same
/// body, so it needs its own re-placement bookkeeping.
pub(super) const CARRIER_WISP: u16 = u16::MAX - 2;

/// Carrier key for `DIG_PLAYERBOX`'s fake remote. Out of the `PlayerId`
/// space on purpose: it borrows a real id for its COLOUR, and keying off
/// that made it share a `Carried::fired` slot with the live player wearing
/// the same id — two carriers, one last-fired position, so both re-placed
/// their light on almost every frame.
pub(super) const CARRIER_DEBUG: u16 = u16::MAX - 1;

/// One remote as this frame sees them.
///
/// `key` and `id` are deliberately separate: the key identifies a
/// CARRIER across frames and the id only picks a colour, which is what
/// lets `DIG_PLAYERBOX` borrow a colour without borrowing a `PlayerId`.
pub(super) struct Remote {
	pub(super) key: u16,
	pub(super) id: u8,
	pub(super) pos: Vec3,
	pub(super) yaw: f32,
	pub(super) held: u16,
	/// The snapshot's packed effect byte (`messages::pack_fx`).
	pub(super) fx: u8,
}

/// Bank a remote's wisp key lives in: a wisp is a SECOND light on the
/// same body (the local player's is [`CARRIER_WISP`]), so it needs its
/// own re-placement slot — the body's key ORed into a range no
/// `PlayerId` or special key reaches.
pub(super) const REMOTE_WISP_BANK: u16 = 0x8000;

/// Everything one remote contributes to this frame: the body box, the
/// item in its hand, and the light that item carries.
///
/// ONE site, so the fake remote `DIG_PLAYERBOX` parks exercises exactly
/// what a real one does — which is the only reason that scene is worth
/// having.
pub(super) fn push_remote(
	bodies: &mut Vec<[f32; 8]>,
	held_out: &mut Vec<(crate::ui::hud::IconKind, [f32; 8])>,
	carried: &mut Carried,
	r: &Remote,
) {
	let Remote { id, pos, yaw, .. } = *r;
	let c = player_color(id);
	bodies.push([pos.x, pos.y, pos.z, yaw, c[0], c[1], c[2], 0.0]);
	// A remote's wisp casts exactly what its caster earned — the level
	// and whitening ride the fx byte because their skills are not in the
	// snapshot, the same reasoning as the held tool's level 0 below.
	if let digblock_net::messages::RemoteFx::Wisp { level, whitened } =
		digblock_net::messages::unpack_fx(r.fx)
	{
		carried.push_wisp(REMOTE_WISP_BANK | r.key, pos, level, whitened);
	}
	let Some(kind) = held_icon(r.held) else {
		return;
	};
	let block = crate::ui::hud::icon_block(kind);
	held_out.push((
		kind,
		[
			pos.x,
			pos.y,
			pos.z,
			yaw,
			f32::from(block.is_some_and(|b| b.props().emissive)),
			crate::item_geom::height(kind) as f32,
			0.0,
			0.0,
		],
	));
	// Level 0: a remote's skills are not in the snapshot, so their tool
	// casts at its row's own level. `carried_light` says why that is the
	// right answer rather than a gap — inventing a rank for someone
	// else's pick would light the room differently for the two of you.
	carried.push(r.key, pos, block, 0);
}

/// How near the camera a carried light has to be for its MOTION to be worth
/// re-placing in the probe field.
///
/// The light itself still reaches the list at any range — this is only about the
/// invalidation sphere, and past this distance the wake it leaves is too dim to
/// read. `GiDraw::invalidate` drains a fixed few spheres a frame and shares that
/// budget with world edits, so a crowd of distant carriers must not spend it.
///
/// This gate is the ONLY bound applied here, deliberately. Past it the queue's
/// own policy is already right: bounded, and dropping the OLDEST, because the
/// newest position is the one whose light is most wrong. A second per-frame cap
/// would either starve a carrier or hand the probes a position its carrier left
/// seconds ago.
pub(super) const CARRIED_INVAL_DIST: f32 = 32.0;

/// How far a carried light must travel before its motion is re-placed in the
/// probe field.
///
/// A DISTANCE since the last fire, deliberately **not** a cell crossing. A
/// crossing test is phase-dependent and pathological: a player resting on a cell
/// boundary, with the sub-block wobble the tick lerp and the decaying
/// reconciliation offset both produce, crosses back and forth every frame.
/// Distance has no such failure mode, is frame-rate independent, needs no
/// hysteresis, and handles a teleport for free.
///
/// One C0 probe spacing, so a probe can never be more than its own cell stale
/// between fires — pinned by `a_carrier_never_outruns_a_probe_cell`.
pub(super) const CARRY_INVAL_STEP: f32 = 2.0;

/// This frame's carried lights, and the bookkeeping that turns a light
/// which MOVES into a light that was re-PLACED.
///
/// The whole design is in that sentence: a torch in a hand is an
/// ordinary entry in the ordinary GI light list, and its motion is
/// expressed through the same hook a block edit uses. Travelling a probe
/// spacing fires `Renderer::gi_invalidate`, which resets probe ages
/// around the new position — so the bounce field re-traces at full
/// weight instead of decaying slowly from a stale value — and caps the
/// resolve's screen history nearby while it does. Standing still fires
/// nothing, so a lit room settles as deeply as one lit from the floor.
#[derive(Default)]
pub(super) struct Carried {
	/// What goes to the GI light list, in `FrameScene::carried` order.
	pub(super) lights: Vec<crate::gi::Emitter>,
	/// Carrier key per entry of `lights`, same index.
	keys: Vec<u16>,
	/// Where each carrier's light last fired an invalidation. A `Vec`
	/// rather than a map because a snapshot holds at most
	/// `MAX_SNAPSHOT_PLAYERS` of these and a linear scan over that many
	/// entries beats hashing one.
	fired: Vec<(u16, Vec3)>,
	/// Positions to invalidate this frame, each with the carrier it
	/// belongs to, drained into the renderer and only then banked by
	/// [`Carried::commit`] — see there for why the two are separate.
	pub(super) pending: Vec<(u16, [f32; 3])>,
}

impl Carried {
	pub(super) fn begin(&mut self) {
		self.lights.clear();
		self.keys.clear();
		self.pending.clear();
	}

	/// Add one carrier if what they hold emits at all.
	///
	/// Derived, never special-cased: any block whose `emits` level is nonzero is
	/// a light in a hand exactly as it is a light on the ground, and it enters
	/// the list under the SAME class byte a placed one does — so tint and cast
	/// level come from one LUT row and a carried torch cannot drift from the
	/// torch beside it.
	///
	/// What it does NOT share is `gi::light_origin`, which is applied CPU-side
	/// in `GiDraw::upload_chunk` and answers "where inside its cell does this
	/// shape glow" — a question a carried light has no cell to ask.
	/// `carried_origin` is the whole answer here. Do not "unify" the two:
	/// removing the offset in upload_chunk silently undoes R26 and puts every
	/// placed torch's light back at its cell centre.
	pub(super) fn push(&mut self, key: u16, eye: Vec3, held: Option<BlockType>, mining: u32) {
		let Some(b) = held.filter(|b| b.props().light > 0) else {
			return;
		};
		let o = digblock_core::gi::carried_origin(eye);
		// What a carried light does NOT take from the class byte: how
		// bright it burns and how blue it stays, when the wielder has
		// earned better. Both ride the entry rather than the LUT, because
		// the same tool in two hands may cast differently — see
		// `core::gi::carried_light`, which is where the rule lives. The
		// level goes out as 0 when it agrees with the row, so an ordinary
		// carrier packs the word R28 packed.
		let cast = digblock_core::gi::carried_light(b, mining);
		self.lights.push(crate::gi::Emitter {
			pos: o.to_array(),
			class: digblock_core::gi::gi_class(digblock_core::chunk::make_cell(b, 0)),
			level: if cast.level == b.props().light {
				0
			} else {
				cast.level
			},
			whitened: cast.whitened,
			// A light in a hand is a point. The one thing that could make
			// it otherwise is the item's own geometry, and an emissive
			// face a few inches across is a point at any distance that
			// matters.
			face: None,
		});
		self.keys.push(key);
	}

	/// The wisp's own entry: arcane light with no item behind it. It
	/// wears the Wisplight row's class — the same LUT row a kindled cell
	/// uses, so the self-light and the placed light are one colour — and
	/// its level and whitening answer to the CASTER's Arcana
	/// (`unlock::wisp_light`), the pick's carried-light rule.
	pub(super) fn push_wisp(&mut self, key: u16, eye: Vec3, level: u8, whitened: bool) {
		self.lights.push(crate::gi::Emitter {
			pos: digblock_core::gi::carried_origin(eye).to_array(),
			class: digblock_core::gi::gi_class(digblock_core::chunk::make_cell(
				BlockType::Wisplight,
				0,
			)),
			level,
			whitened,
			face: None,
		});
		self.keys.push(key);
	}

	/// Close the frame: note which carriers have travelled far enough to
	/// be re-placed, and which have STOPPED being lights.
	///
	/// A carrier that is NEW fires immediately — the probes around it
	/// have never seen this light — which is what makes selecting a torch
	/// light the room without waiting for a step.
	pub(super) fn finish(&mut self, cam: Vec3) {
		for (i, &key) in self.keys.iter().enumerate() {
			let p = Vec3::from_array(self.lights[i].pos);
			let slot = self.fired.iter().position(|(k, _)| *k == key);
			if let Some(s) = slot
				&& self.fired[s].1.distance_squared(p) < CARRY_INVAL_STEP * CARRY_INVAL_STEP
			{
				continue;
			}
			// One sphere, not two. A step is far shorter than the sphere
			// `invalidate` queues in either cascade, so the space
			// just vacated is covered many times over; only a jump longer
			// than the radius would need the departure point, and nothing
			// in the game moves a player that far without a teleport,
			// which resets the whole screen history anyway.
			if cam.distance_squared(p) <= CARRIED_INVAL_DIST * CARRIED_INVAL_DIST {
				self.pending.push((key, p.to_array()));
			}
		}
		// A light going OUT is a lighting change exactly as a light
		// arriving is, and it is the half that was missing: `flush_lights`
		// drops the entry from the GPU list the same frame, so the direct
		// pool dies at once — but the probes keep its bounce and decay it
		// over seconds, so a room stayed lit by a torch that had been put
		// away. Digging a PLACED torch has always invalidated, through
		// `take_gi_edits`, which is precisely the asymmetry a carried
		// light is not supposed to have. Covers a remote disconnecting
		// or falling out of the snapshot too.
		let keys = &self.keys;
		let (cam, pending) = (cam, &mut self.pending);
		self.fired.retain(|&(k, p)| {
			if keys.contains(&k) {
				return true;
			}
			if cam.distance_squared(p) <= CARRIED_INVAL_DIST * CARRIED_INVAL_DIST {
				pending.push((k, [p.x, p.y, p.z]));
			}
			false
		});
	}

	/// Bank the positions that actually reached the renderer.
	///
	/// Separate from `finish` because `pending` is drained inside the
	/// `renderer.is_some()` arm: banking at queue time meant that a frame
	/// with no renderer — device loss, or the wait for the GPU at startup
	/// — recorded a fire that never happened, and the carrier then sat
	/// stale until it had walked another whole `CARRY_INVAL_STEP`.
	///
	/// A departed carrier is not re-banked: its key is gone from `fired`
	/// and its light no longer exists, so there is nothing left to
	/// compare a future position against.
	pub(super) fn commit(&mut self) {
		for &(key, p) in &self.pending {
			let p = Vec3::from_array(p);
			match self.fired.iter().position(|(k, _)| *k == key) {
				Some(s) => self.fired[s].1 = p,
				None if self.keys.contains(&key) => self.fired.push((key, p)),
				None => {}
			}
		}
	}
}

/// Pack linear RGB into the overlay's 0xAABBGGRR vertex color (opaque).
pub(super) fn pack_rgb(rgb: [f32; 3]) -> u32 {
	let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
	0xFF00_0000 | (c(rgb[2]) << 16) | (c(rgb[1]) << 8) | c(rgb[0])
}

/// World-space center of a block cell (positional-audio anchor).
pub(super) fn cell_center(cell: (i32, i32, i32)) -> [f32; 3] {
	[
		cell.0 as f32 + 0.5,
		cell.1 as f32 + 0.5,
		cell.2 as f32 + 0.5,
	]
}

#[cfg(test)]
mod carried_tests {
	use super::*;

	fn torch_at(c: &mut Carried, key: u16, eye: Vec3) {
		c.push(key, eye, Some(BlockType::Torch), 1);
	}

	/// One frame of one carrier holding a torch, in exactly the order
	/// `Engine::render` runs it — `commit` included, since that is what
	/// banks the fire, and without it every frame looks like the first.
	/// Answers how many spheres the frame queued.
	fn frame(c: &mut Carried, key: u16, eye: Vec3, cam: Vec3) -> usize {
		frame_holding(c, key, eye, cam, Some(BlockType::Torch))
	}

	/// The same, for a hand holding anything (or nothing).
	fn frame_holding(
		c: &mut Carried,
		key: u16,
		eye: Vec3,
		cam: Vec3,
		held: Option<BlockType>,
	) -> usize {
		c.begin();
		c.push(key, eye, held, 1);
		c.finish(cam);
		let n = c.pending.len();
		c.commit();
		n
	}

	/// A carried light must be re-placed no less often than the bounce
	/// field can resolve, or a carrier outruns a whole probe cell between
	/// fires and the cache holds light from somewhere they have left.
	#[test]
	fn a_carrier_never_outruns_a_probe_cell() {
		assert!(
			CARRY_INVAL_STEP <= crate::gi::C0_SPACING as f32,
			"the step must stay inside C0's own spacing"
		);
	}

	/// The whole cost model rests on this: a player standing still with a
	/// torch is not a moving emitter, so nothing is invalidated and the
	/// room they are in converges as deeply as one lit from the floor.
	///
	/// It is also why the trigger is a DISTANCE and not a cell crossing.
	/// The second half of this test is the pathology a crossing rule
	/// has: a carrier wobbling either side of a cell boundary — which is
	/// what the tick lerp and the decaying reconciliation offset produce
	/// — would fire on every single frame.
	#[test]
	fn a_still_carrier_is_not_a_moving_emitter() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let mut c = Carried::default();
		for f in 0..600 {
			// The first frame places the light: the probes around it have
			// never seen it, which is what lights the room the moment you
			// select a torch.
			let want = usize::from(f == 0);
			assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), want, "frame {f}");
		}
		// Straddling a cell boundary, a hundredth of a block either way.
		let edge = Vec3::new(64.0, 70.5, 64.0);
		let mut c = Carried::default();
		frame(&mut c, CARRIER_SELF, edge, edge);
		for f in 0..600 {
			let p = edge + Vec3::Z * if f % 2 == 0 { 0.01 } else { -0.01 };
			assert_eq!(frame(&mut c, CARRIER_SELF, p, p), 0, "wobble fired on {f}");
		}
	}

	/// Walking fires once per step travelled, not once per frame — the
	/// probes around the carrier re-trace at full weight instead of
	/// decaying from a stale value.
	#[test]
	fn a_walking_carrier_fires_once_per_step() {
		let start = Vec3::new(64.0, 70.0, 64.0);
		let per_frame = digblock_core::player::WALK_SPEED / 60.0;
		let mut c = Carried::default();
		let mut fires = 0usize;
		let frames = 300; // five seconds
		for f in 0..frames {
			let p = start + Vec3::Z * (per_frame * f as f32);
			fires += frame(&mut c, CARRIER_SELF, p, p);
		}
		let walked = per_frame * (frames - 1) as f32;
		// One placement plus one per step of travel.
		let want = 1 + (walked / CARRY_INVAL_STEP) as usize;
		assert_eq!(fires, want, "walked {walked} blocks");
	}

	/// Only a block that EMITS is carried, and the answer is derived from
	/// the block table rather than named — so a second emitter needs no
	/// edit here, and the bucket (whose charge is Water, and whose empty
	/// state decodes to a tool sentinel) is never a light.
	#[test]
	fn only_an_emitting_block_lights_a_hand() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		for b in BlockType::ALL {
			let mut c = Carried::default();
			c.begin();
			c.push(CARRIER_SELF, eye, Some(b), 1);
			assert_eq!(
				c.lights.len(),
				usize::from(b.props().light > 0),
				"{b:?} light {}",
				b.props().light
			);
		}
		let mut c = Carried::default();
		c.begin();
		c.push(CARRIER_SELF, eye, None, 1);
		assert!(c.lights.is_empty(), "an empty hand is not a light");
	}

	/// A carried torch enters the list under the same class byte a placed
	/// one does, so tint and cast level come from one LUT row. NOT the
	/// flame offset — that is `gi::light_origin`, applied CPU-side to
	/// chunk emitters only, and a carried light has no cell to offset
	/// within; see `Carried::push`.
	#[test]
	fn a_carried_torch_borrows_the_placed_torchs_class() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let mut c = Carried::default();
		c.begin();
		torch_at(&mut c, CARRIER_SELF, eye);
		let want =
			digblock_core::gi::gi_class(digblock_core::chunk::make_cell(BlockType::Torch, 0));
		assert_eq!(c.lights[0].class, want);
		assert!(
			want >= digblock_core::gi::SHAPE_CLASS_BASE,
			"the shaped class"
		);
		let o = digblock_core::gi::carried_origin(eye);
		assert_eq!(c.lights[0].pos, o.to_array());
		// And no override: a torch is a torch in anyone's hand, whatever
		// they have earned.
		assert_eq!(c.lights[0].level, 0);
	}

	/// The one light in the game that answers to its wielder. Below the
	/// pick's plane rank the entry defers to the LUT exactly as a torch
	/// does; at it, the entry carries a level of its own — the rank the
	/// player just felt widen the swing is the rank that brightens the
	/// tool.
	#[test]
	fn the_pick_brightens_at_the_rank_that_widens_it() {
		use digblock_core::unlock::PICK_PLANE_AT;
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let base = BlockType::ResonantPick.props().light;
		let level_at = |mining| {
			let mut c = Carried::default();
			c.begin();
			c.push(CARRIER_SELF, eye, Some(BlockType::ResonantPick), mining);
			c.lights[0].level
		};
		// The rank boundary is read from `unlock`, never restated: one
		// threshold, and this test moves with it.
		let plane = PICK_PLANE_AT;
		assert_eq!(
			level_at(plane - 1),
			0,
			"below the rank, the row's own level"
		);
		let raised = level_at(plane);
		assert!(
			raised > base,
			"at the rank it casts brighter than its row ({raised} vs {base})"
		);
		// It out-LEVELS a torch here, and still lights a room a fraction
		// as brightly, because what an emitter delivers is its level
		// times its tint's luminance and this one is a saturated blue —
		// `texture::layer_tests::a_tool_lights_less_than_a_torch` is
		// where that can be checked against the actual pixels.
		assert!(
			u32::from(raised) <= crate::gi::LIGHT_LEVEL_MASK,
			"a level has to fit the light entry's own field"
		);
	}

	/// A carrier far from the camera still LIGHTS — it is in the list —
	/// but its motion does not spend an invalidation sphere, because the
	/// four a frame belong to world edits and a wake that far out cannot
	/// be read.
	#[test]
	fn a_distant_carriers_motion_costs_nothing() {
		let cam = Vec3::new(64.0, 70.0, 64.0);
		let far = cam + Vec3::X * (CARRIED_INVAL_DIST + 4.0);
		let mut c = Carried::default();
		for f in 0..120 {
			let p = far + Vec3::Z * (f as f32 * 0.5);
			assert_eq!(frame(&mut c, 3, p, cam), 0, "spent a sphere on frame {f}");
			assert_eq!(c.lights.len(), 1, "still a light on frame {f}");
		}
	}

	/// A light going OUT is a lighting change exactly as one arriving is.
	///
	/// `flush_lights` drops the entry the same frame, so the direct pool
	/// dies at once — but the probes hold its bounce and decay it over
	/// seconds, so without this a room stayed lit by a torch that had
	/// been put away. Digging a PLACED torch has always invalidated,
	/// through `take_gi_edits`, and the whole R28 thesis is that a
	/// carried light is not a second kind of thing.
	#[test]
	fn putting_a_torch_away_invalidates_where_it_was() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let gone = digblock_core::gi::carried_origin(eye).to_array();
		let mut c = Carried::default();
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 1, "selecting lights");
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 0, "then settles");
		// Swap to something that does not emit: one sphere, at the place
		// the light was, and then silence.
		c.begin();
		c.push(CARRIER_SELF, eye, Some(BlockType::Dirt), 1);
		c.finish(eye);
		assert_eq!(c.pending.len(), 1, "putting it away must invalidate");
		assert_eq!(c.pending[0].1, gone, "at the position it lit from");
		c.commit();
		assert_eq!(
			frame_holding(&mut c, CARRIER_SELF, eye, eye, Some(BlockType::Dirt)),
			0,
			"and not again every frame afterwards"
		);
		// Re-selecting it is a new light, so it places again.
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 1, "re-selecting");
	}

	/// The same for a carrier who simply vanishes — a remote
	/// disconnecting, or dropping out of the snapshot.
	#[test]
	fn a_departed_carrier_invalidates_where_it_was() {
		let cam = Vec3::new(64.0, 70.0, 64.0);
		let eye = cam + Vec3::X * 6.0;
		let mut c = Carried::default();
		assert_eq!(frame(&mut c, 7, eye, cam), 1);
		// Nobody at all this frame.
		c.begin();
		c.finish(cam);
		assert_eq!(c.pending.len(), 1, "a carrier who left takes their light");
		c.commit();
		c.begin();
		c.finish(cam);
		assert!(c.pending.is_empty(), "and is then forgotten");
	}

	/// A frame that never reached the renderer must not be banked as a
	/// fire, or the carrier sits stale until it has walked another whole
	/// step. `commit` is what separates queued from applied.
	#[test]
	fn an_unapplied_fire_is_retried() {
		let eye = Vec3::new(64.0, 70.0, 64.0);
		let mut c = Carried::default();
		for _ in 0..5 {
			// finish() without commit(): the renderer was absent (device
			// loss, or the wait for the GPU at startup).
			c.begin();
			torch_at(&mut c, CARRIER_SELF, eye);
			c.finish(eye);
			assert_eq!(c.pending.len(), 1, "still owed an invalidation");
		}
		c.commit();
		assert_eq!(frame(&mut c, CARRIER_SELF, eye, eye), 0, "banked once");
	}

	/// The fake remote `DIG_PLAYERBOX` parks must not share a carrier key
	/// with a real player: two carriers on one `fired` slot overwrite each
	/// other's last-fired position, so both re-place on nearly every
	/// frame.
	#[test]
	fn the_debug_carrier_key_is_nobodys_player_id() {
		assert!(u16::from(u8::MAX) < CARRIER_DEBUG);
		assert_ne!(CARRIER_DEBUG, CARRIER_SELF);
	}

	/// A remote whose fx byte carries a wisp is a light — at exactly the
	/// level and whitening the byte says, under the Wisplight row's class,
	/// keyed into the wisp bank so it never shares a `fired` slot with the
	/// same body's held item. The whole point of the fx byte: their skills
	/// are not in the snapshot, so the light rides the wire instead.
	#[test]
	fn a_remotes_wisp_fx_casts_what_the_byte_says() {
		use digblock_net::messages::{RemoteFx, pack_fx};
		let pos = Vec3::new(64.0, 70.0, 64.0);
		let mut c = Carried::default();
		c.begin();
		let mut bodies = Vec::new();
		let mut held = Vec::new();
		let (level, whitened) = (9u8, true);
		push_remote(
			&mut bodies,
			&mut held,
			&mut c,
			&Remote {
				key: 3,
				id: 3,
				pos,
				yaw: 0.0,
				held: u16::MAX,
				fx: pack_fx(RemoteFx::Wisp { level, whitened }),
			},
		);
		assert_eq!(c.lights.len(), 1, "an empty hand, but a burning wisp");
		let want =
			digblock_core::gi::gi_class(digblock_core::chunk::make_cell(BlockType::Wisplight, 0));
		assert_eq!(c.lights[0].class, want);
		assert_eq!(c.lights[0].level, level);
		assert!(c.lights[0].whitened);
		assert_eq!(c.keys[0], REMOTE_WISP_BANK | 3, "its own fired slot");
		// And fx = 0 is nobody's wisp.
		let mut c = Carried::default();
		c.begin();
		push_remote(
			&mut bodies,
			&mut held,
			&mut c,
			&Remote {
				key: 3,
				id: 3,
				pos,
				yaw: 0.0,
				held: u16::MAX,
				fx: 0,
			},
		);
		assert!(c.lights.is_empty());
	}
}
