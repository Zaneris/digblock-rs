use super::*;
use digblock_core::block::BlockType;

const ASPECT: f32 = 16.0 / 9.0;

/// One vertex word, two readers — your own hand and a remote's — and
/// nothing but agreement makes them one item. Both shaders must
/// declare the same bit, and it must sit clear of every texture layer
/// there will ever be, since they share the word.
#[test]
fn the_ink_bit_matches_the_shaders() {
	let ink = format!("const ITEM_INK_BIT = {}u;", 1u32 << INK_BIT);
	let finish = format!("const ITEM_FINISH_SHIFT = {FINISH_SHIFT}u;");
	for (name, src) in [
		("viewmodel", digblock_assets::shader::VIEWMODEL),
		("players", digblock_assets::shader::PLAYERS),
	] {
		assert!(src.contains(&ink), "{name}.wgsl must declare `{ink}`");
		assert!(src.contains(&finish), "{name}.wgsl must declare `{finish}`");
	}
	assert!(
		u32::from(digblock_core::block::tex::COUNT) < 1u32 << INK_BIT,
		"the layer index has to fit under the bit it shares a word with"
	);
}

/// The block in your hand and the block on the floor are one material: every face of a held
/// item carries the finish its texture wears in the mesher, so the two cannot disagree about
/// which of them gleams.
#[test]
fn a_held_block_wears_its_placed_finish() {
	let mut verts = Vec::new();
	for kind in [
		IconKind::Cube(BlockType::Metal),
		IconKind::Shaped(BlockType::MetalStairs),
		IconKind::Cube(BlockType::PolishedStone),
		IconKind::Shaped(BlockType::CrystalSlab),
		IconKind::Cube(BlockType::Stone),
	] {
		build(kind, &mut verts);
		assert!(!verts.is_empty(), "{kind:?} drew nothing");
		for v in &verts {
			let layer = (v.layer & ((1 << INK_BIT) - 1)) as u16;
			assert_eq!(
				(v.layer >> FINISH_SHIFT) & 3,
				digblock_core::block::tex::finish(layer),
				"{kind:?}: a face's finish disagrees with its texture's"
			);
		}
	}
}

/// A tool's lit part is a FACE, not the item: the geometry has to
/// carry both kinds at once, or a glowing core costs the body its
/// shading.
#[test]
fn a_tool_glows_in_one_place_and_is_shaded_everywhere_else() {
	for kind in [IconKind::Siphon, IconKind::Pick] {
		let faces = crate::item_geom::faces(kind, |f| f.clone());
		assert!(
			faces.iter().any(|f| f.emissive),
			"{kind:?} has no lit part — it is a light source with nothing to see"
		);
		assert!(
			faces.iter().any(|f| !f.emissive),
			"{kind:?} is fullbright throughout, which is the flat look the \
				 per-face bit exists to avoid"
		);
		// The lit part wears the row's own texture, which is where the
		// GI takes the colour of the light from: one authored thing,
		// so the glow you see and the glow it throws cannot drift.
		let block = crate::ui::hud::icon_block(kind).expect("a tool has a row");
		assert!(
			faces
				.iter()
				.filter(|f| f.emissive)
				.all(|f| f.tex_layer == block.props().tex_top),
			"{kind:?}'s glow is not its row's texture"
		);
		assert!(block.props().light > 0, "{kind:?} does not cast");
		assert!(
			!block.props().emissive,
			"{kind:?} is fullbright as a BLOCK, which would flatten the whole tool"
		);
	}
}

/// Project an item-space point (twelfths) to normalized device
/// coordinates, so the tests can ask where the thing actually is.
fn ndc(aspect: f32, height: i32, pose: Pose, p: [f32; 3]) -> [f32; 3] {
	let c = mvp(aspect, height, pose) * glam::Vec4::new(p[0], p[1], p[2], 1.0);
	[c.x / c.w, c.y / c.w, c.z / c.w]
}

fn centre(aspect: f32, height: i32, pose: Pose) -> [f32; 3] {
	ndc(aspect, height, pose, [6.0, height as f32 / 2.0, 6.0])
}

/// A torch in your hand is a lit torch, and a dirt block is not — and
/// neither answer is written down anywhere: both fall out of the
/// block's own `glows` row, so a second emitter needs no edit here.
#[test]
fn a_torch_in_hand_is_ink_and_a_dirt_block_is_not() {
	let frame = |kind| ViewmodelFrame {
		kind,
		pose: Pose::default(),
		light_dir_view: Vec3::new(0.0, -1.0, 0.0),
		light_strength: 1.0,
		light_color: [1.0, 1.0, 1.0],
	};
	let ink = |kind| uniforms(ASPECT, &frame(kind)).light_color[3];
	assert_eq!(ink(IconKind::Shaped(BlockType::Torch)), 1.0);
	assert_eq!(ink(IconKind::Cube(BlockType::Dirt)), 0.0);
	assert_eq!(ink(IconKind::Shaped(BlockType::WoodStairs)), 0.0);
	// The pail is planed timber and its charge is water; neither
	// glows, in either state.
	assert_eq!(ink(IconKind::WaterTool { full: false }), 0.0);
	assert_eq!(ink(IconKind::WaterTool { full: true }), 0.0);
}

#[test]
fn the_item_rests_in_the_lower_right_and_stays_on_screen() {
	for aspect in [1.0f32, 4.0 / 3.0, 16.0 / 9.0, 21.0 / 9.0] {
		for height in [12, 24] {
			let c = centre(aspect, height, Pose::default());
			assert!(c[0] > 0.1 && c[0] < 0.95, "x {} at {aspect}", c[0]);
			assert!(c[1] < -0.1 && c[1] > -0.95, "y {} at {aspect}", c[1]);
			assert!(c[2] > 0.0 && c[2] < 1.0, "in front of the eye: {}", c[2]);
		}
	}
}

/// The whole point of the item's own projection: the world's FOV,
/// the window shape and the near plane are all somebody else's
/// problem. Its on-screen HEIGHT must not move with the aspect.
#[test]
fn on_screen_size_is_independent_of_the_window_shape() {
	let span = |aspect: f32| {
		let lo = ndc(aspect, 12, Pose::default(), [6.0, 0.0, 6.0]);
		let hi = ndc(aspect, 12, Pose::default(), [6.0, 12.0, 6.0]);
		(hi[1] - lo[1]).abs()
	};
	let want = span(16.0 / 9.0);
	for aspect in [1.0f32, 4.0 / 3.0, 21.0 / 9.0] {
		assert!(
			(span(aspect) - want).abs() < 1e-4,
			"{aspect}: {} vs {want}",
			span(aspect)
		);
	}
}

/// A door is twice as tall as a cube; both hang centred on the same
/// spot rather than a door sinking out of frame.
#[test]
fn tall_items_stay_centred_on_the_rest_position() {
	let cube = centre(ASPECT, 12, Pose::default());
	let door = centre(ASPECT, 24, Pose::default());
	for i in 0..3 {
		assert!((cube[i] - door[i]).abs() < 1e-5, "axis {i}");
	}
}

/// Icon and hand must agree about which way an item faces: both
/// show +x, +y and +z. Checked through the transform rather than by
/// asserting the constants back at themselves.
#[test]
fn the_hand_shows_the_same_three_faces_as_the_icon() {
	let m = mvp(ASPECT, 12, Pose::default());
	let origin = m * glam::Vec4::new(6.0, 6.0, 6.0, 1.0);
	let o = glam::Vec3::new(origin.x, origin.y, origin.z) / origin.w;
	for axis in [
		glam::Vec3::X * 6.0,
		glam::Vec3::Y * 6.0,
		glam::Vec3::Z * 6.0,
	] {
		let p = m * (glam::Vec4::from((glam::Vec3::new(6.0, 6.0, 6.0) + axis, 1.0)));
		let p = glam::Vec3::new(p.x, p.y, p.z) / p.w;
		assert!(
			p.z < o.z,
			"{axis:?} should come toward the viewer ({} vs {})",
			p.z,
			o.z
		);
	}
}

#[test]
fn the_bob_settles_when_you_stop_and_never_leaves_its_box() {
	let mut bob = Bob::default();
	let dt = 1.0 / 60.0;
	// Walk for a second: the amplitude comes up and the pose moves.
	for _ in 0..60 {
		bob.advance(4.0 * dt, dt, true);
	}
	let walking = bob.pose();
	assert!(bob.amp > 0.9, "full travel at walk speed: {}", bob.amp);
	// Bounded travel, always — a bob that can grow is a bob that
	// eventually posts the item off-screen. The envelopes sum to
	// one, so the walk-plus-breath total can never exceed either
	// budget added together, whatever the crossfade is doing.
	for _ in 0..600 {
		bob.advance(4.0 * dt, dt, true);
		let p = bob.pose();
		assert!(p.offset.x.abs() <= BOB_X + BREATH_X + 1e-5);
		assert!(p.offset.y.abs() <= BOB_Y + BREATH_Y + 1e-5);
		assert!(p.roll.abs() <= BOB_ROLL + 1e-5);
	}
	// At full walk the breath is squeezed out entirely, so the bob's
	// own bound still holds on its own.
	assert!(bob.pose().offset.x.abs() <= BOB_X + 1e-3);
	// Standing still: the stride settles out and the breath takes
	// over the amplitude it gives up.
	for _ in 0..180 {
		bob.advance(0.0, dt, true);
	}
	assert!(bob.amp < 0.01, "stride settled: {}", bob.amp);
	assert!(walking.offset.length() > 0.05, "it did move while walking");
}

/// A perfectly still hand reads as a decal stuck to the screen. The
/// point of the breath is that it keeps moving with no input at all
/// — which is exactly what a distance-driven bob cannot do.
#[test]
fn the_hand_keeps_breathing_when_nothing_moves() {
	let mut bob = Bob::default();
	let dt = 1.0 / 60.0;
	let mut lo = f32::MAX;
	let mut hi = f32::MIN;
	// Two full cycles of standing perfectly still.
	for _ in 0..(2.0 * BREATH_SECS / dt) as usize {
		bob.advance(0.0, dt, true);
		let y = bob.pose().offset.y;
		lo = lo.min(y);
		hi = hi.max(y);
	}
	assert!(
		hi - lo > BREATH_Y,
		"the idle hand should drift a full breath ({lo}..{hi})"
	);
	assert!(
		hi <= BREATH_Y + 1e-5 && lo >= -BREATH_Y - 1e-5,
		"and no more"
	);
	// That it stays a drift rather than a second bob is a relation
	// between two constants, so it is asserted at compile time up
	// where they are declared.
}

/// The exchange must happen where you can't see it. If `shown`
/// flipped at the top of the arc it would be a jump-cut with a dip
/// bolted on, which is the thing this replaces.
#[test]
fn a_swap_changes_the_item_at_the_bottom_of_the_drop() {
	let dt = 1.0 / 240.0;
	let (dirt, stone) = (
		IconKind::Cube(BlockType::Dirt),
		IconKind::Cube(BlockType::Stone),
	);
	let mut s = Swap::default();
	// First item: nothing to put away, so it only rises.
	s.advance(dt, Some(dirt));
	assert_eq!(s.shown(), Some(dirt), "an empty hand skips the drop");
	for _ in 0..60 {
		s.advance(dt, Some(dirt));
	}
	assert_eq!(s.pose(), Pose::default(), "and settles at rest");

	// Now a real swap. The old item is held until the item is off
	// screen, and the drop is deep enough to have taken it there.
	let mut deepest: f32 = 0.0;
	let mut swapped_at: f32 = -1.0;
	for i in 0..60 {
		s.advance(dt, Some(stone));
		let y = s.pose().offset.y;
		deepest = deepest.min(y);
		if swapped_at < 0.0 && s.shown() == Some(stone) {
			swapped_at = -y;
		}
		assert!(i > 0 || s.shown() == Some(dirt), "not on the first frame");
	}
	assert_eq!(s.shown(), Some(stone));
	assert!(
		(deepest + SWAP_DROP).abs() < 0.01,
		"the arc reaches full depth: {deepest}"
	);
	assert!(
		swapped_at > SWAP_DROP * 0.9,
		"the exchange happened near the bottom, not on the way ({swapped_at})"
	);
	// Back at rest afterwards.
	for _ in 0..60 {
		s.advance(dt, Some(stone));
	}
	assert_eq!(s.pose(), Pose::default());
}

/// Filling the bucket changes the item DRAWN but not the item HELD.
/// A dip there would read as "you swapped slots" every time a scoop
/// landed, which is the opposite of what just happened.
#[test]
fn charging_the_bucket_shows_the_water_without_a_dip() {
	let dt = 1.0 / 60.0;
	let (empty, full) = (
		IconKind::WaterTool { full: false },
		IconKind::WaterTool { full: true },
	);
	let mut s = Swap::default();
	for _ in 0..60 {
		s.advance(dt, Some(empty));
	}
	assert_eq!(s.shown(), Some(empty));
	assert_eq!(s.pose(), Pose::default(), "at rest before the scoop");
	s.advance(dt, Some(full));
	assert_eq!(s.shown(), Some(full), "the water appears immediately");
	assert_eq!(s.pose(), Pose::default(), "and the hand never moved");
	// A real slot change from the charged bucket still dips.
	s.advance(dt, Some(IconKind::Cube(BlockType::Dirt)));
	assert_ne!(s.pose(), Pose::default());
}

/// Scrolling the bar shouldn't teleport the item back to the top
/// between slots — each change continues from where the hand is.
#[test]
fn scrolling_through_slots_never_snaps_the_item_upward() {
	let dt = 1.0 / 240.0;
	let mut s = Swap::default();
	s.advance(dt, Some(IconKind::Cube(BlockType::Dirt)));
	for _ in 0..60 {
		s.advance(dt, Some(IconKind::Cube(BlockType::Dirt)));
	}
	// A new slot every few frames: fast, but slower than the swap, so
	// each lands mid-rise — the case that used to snap.
	let items = [BlockType::Stone, BlockType::Wood, BlockType::Gravel];
	let mut prev = s.pose().offset.y;
	for (n, b) in (0..180).zip(items.iter().cycle()) {
		let want = IconKind::Cube(*b);
		s.advance(dt, Some(if n % 6 == 0 { want } else { s.shown().unwrap() }));
		let y = s.pose().offset.y;
		// Per-frame travel can never exceed the arc's own peak rate;
		// a snap back to rest would blow straight past it.
		let peak_rate = SWAP_DROP * std::f32::consts::PI / SWAP_SECS * dt;
		assert!(
			(y - prev).abs() <= peak_rate + 1e-4,
			"frame {n}: jumped {} (peak {peak_rate})",
			y - prev
		);
		prev = y;
	}
}

/// One scoop, one stroke. The bug this pins: the arc was shorter
/// than the hold, so it ran out a few ticks early, a second arc
/// started, and one bucketful looked like two grabs at the water.
#[test]
fn a_scoop_is_exactly_one_swing() {
	let mut s = Swing::default();
	let dt = 1.0 / 60.0;
	assert_eq!(s.pose(), Pose::default(), "idle hands are still");
	// The hold opens. Count the strokes over a whole scoop by
	// watching the arc leave and return to rest.
	s.strike();
	let (mut strokes, mut peak, mut was_out) = (0, 0.0f32, false);
	for _ in 0..(SWING_SECS / dt).ceil() as usize {
		s.advance(dt);
		let out = s.pose() != Pose::default();
		strokes += usize::from(out && !was_out);
		was_out = out;
		peak = peak.max(s.pose().offset.y.abs());
	}
	assert_eq!(strokes, 1, "one stroke for one bucket");
	assert!(peak > SWING_Y * 0.9, "the arc reaches its travel: {peak}");
	// And the scoop lands with the hand back where it started.
	assert_eq!(s.pose(), Pose::default(), "at rest as the water arrives");
	// Nothing re-arms it: holding the button past the scoop is a
	// bucket that is already full, not another swing.
	for _ in 0..120 {
		s.advance(dt);
		assert_eq!(s.pose(), Pose::default(), "no arc without a strike");
	}
	// Releasing mid-arc still runs the current stroke out rather
	// than snapping the hand back.
	s.strike();
	s.advance(dt);
	assert!(s.pose() != Pose::default(), "mid-arc, still swinging");
	for _ in 0..60 {
		s.advance(dt);
	}
	assert_eq!(s.pose(), Pose::default(), "and then it rests");
}

/// The arc's length is a duration shared with core — the water
/// block's own hold time. Retuning the scoop without retuning this
/// brings the double-swipe straight back.
#[test]
fn the_swing_is_exactly_one_scoop_long() {
	let ticks = BlockType::Water
		.hardness_ticks()
		.expect("water is scoopable");
	let scoop = ticks as f32 * digblock_core::player::TICK;
	assert!(
		(SWING_SECS - scoop).abs() < 1e-6,
		"the swing is {SWING_SECS}s but a scoop takes {scoop}s"
	);
}

/// The hand keeps time with the feet, so a jump stills it — the
/// same rule the footstep cadence follows. Sailing through the air
/// at walking speed used to swing the item at a full walking pace.
#[test]
fn the_bob_stops_swinging_in_the_air() {
	let mut bob = Bob::default();
	let dt = 1.0 / 60.0;
	for _ in 0..60 {
		bob.advance(4.0 * dt, dt, true);
	}
	assert!(bob.amp > 0.9, "walking first: {}", bob.amp);
	let (phase, mut peak) = (bob.phase, 0.0f32);
	// Same speed, now airborne: the swing settles out inside a
	// stride's worth of travel, and the phase waits where it was.
	for _ in 0..60 {
		bob.advance(4.0 * dt, dt, false);
	}
	assert!(bob.amp < 0.01, "the stride settled: {}", bob.amp);
	assert_eq!(bob.phase, phase, "and kept its place for the landing");
	// What's left is the breath, which is a quarter of the travel.
	for _ in 0..60 {
		bob.advance(4.0 * dt, dt, false);
		peak = peak.max(bob.pose().offset.y.abs());
	}
	assert!(peak <= BREATH_Y + 1e-5, "only the breath is left: {peak}");
}

/// The shading rig has to survive the trip into item space: rotating
/// the answer BACK by the pose's forward rotation must land on the
/// view-space light exactly. A round trip, because the bug this frame
/// change invites — inverting the wrong way, so "from above" shades
/// from below — preserves length and passes any magnitude check.
#[test]
fn the_item_space_light_round_trips_through_the_pose_rotation() {
	let d = Vec3::new(0.3, -0.9, 0.2).normalize();
	for pose in [
		Pose::default(),
		Pose {
			offset: Vec3::ZERO,
			pitch: 0.5,
			roll: -0.2,
		},
	] {
		let l = light_in_item_space(d, pose);
		let forward = Mat4::from_rotation_z(pose.roll)
			* Mat4::from_rotation_x(REST_PITCH + pose.pitch)
			* Mat4::from_rotation_y(REST_YAW);
		let back = forward.transform_vector3(l);
		assert!((back - d).length() < 1e-5, "{back} back vs {d}");
	}
}

/// players.rs's pipeline describes `Vert` as four attributes at byte
/// offsets it writes as literals — a WGSL-mirror-style contract, so the
/// offsets are pinned HERE, where the fields are visible. Size and
/// alignment alone once stood for this, and they cannot see two fields
/// swapping places.
#[test]
fn the_held_vertex_layout_matches_the_pipeline() {
	assert_eq!(std::mem::size_of::<Vert>(), 28);
	assert_eq!(std::mem::align_of::<Vert>(), 4);
	assert_eq!(std::mem::offset_of!(Vert, pos), 0);
	assert_eq!(std::mem::offset_of!(Vert, uv), 12);
	assert_eq!(std::mem::offset_of!(Vert, layer), 20);
	assert_eq!(std::mem::offset_of!(Vert, face), 24);
}
