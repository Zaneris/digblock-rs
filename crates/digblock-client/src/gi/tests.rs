use super::*;

/// `flush_lights` drops emitters outside the reach every march breaks
/// at, so its bound has to BE the shader's. Read `gi_lateral_max` out
/// of gi_dda.wgsl rather than restating it: a hand-copied twin is how
/// this codebase found the same stepper bug three times, and drifting
/// here silently hides a torch the shader would have traced.
#[test]
fn the_light_reach_matches_the_shaders_bound() {
	let src = digblock_assets::shader::GI_DDA;
	let body = src
		.split("fn gi_lateral_max(side: f32) -> f32 {")
		.nth(1)
		.expect("gi_dda.wgsl must declare gi_lateral_max(side)");
	let expr = body
		.split(';')
		.next()
		.expect("gi_lateral_max must have a return")
		.replace("return", "")
		.replace(char::is_whitespace, "");
	assert_eq!(
		expr, "side*0.5-1.0",
		"gi_lateral_max changed shape — update `lateral_max` to match"
	);
	for wc in [8u32, 20, 32] {
		let side = (wc * 16) as f32;
		assert_eq!(lateral_max(wc), side * 0.5 - 1.0, "window {wc}");
	}
}

/// The unproven-daylight fade (R32) is bit-identical above its top
/// edge because `smoothstep` saturates there — which is only the
/// surface-world guarantee while that edge IS the surface's floor.
/// This crate is the one that sees both the shader source and
/// worldgen, so it holds the two together: the ramp tops out at
/// `SEA_LEVEL`, and its bottom edge sits below its top (a degenerate
/// or inverted smoothstep would fade nothing, or everything).
#[test]
fn the_sky_fade_tops_out_at_sea_level() {
	let src = digblock_assets::shader::MEDIUM_COMMON;
	let wgsl_const = |name: &str| -> f64 {
		let pat = format!("const {name} = ");
		let at = src
			.find(&pat)
			.unwrap_or_else(|| panic!("{name}: not declared in medium_common.wgsl"))
			+ pat.len();
		let rest = &src[at..];
		let end = rest.find(';').expect("unterminated const");
		rest[..end].trim().parse().expect("not a number")
	};
	let full = wgsl_const("SKY_FADE_FULL_Y");
	let zero = wgsl_const("SKY_FADE_ZERO_Y");
	assert_eq!(
		full,
		f64::from(digblock_core::worldgen::SEA_LEVEL),
		"SKY_FADE_FULL_Y must equal SEA_LEVEL — above it the fade must saturate \
			 so the surface world cannot change"
	);
	assert!(
		zero < full,
		"SKY_FADE_ZERO_Y ({zero}) must sit below SKY_FADE_FULL_Y ({full})"
	);
}

/// A carried torch is an entry in the ordinary light list, so the
/// only thing that needs pinning is that the ordinary rules land it
/// in the right place: nearest-first sorting must make the carrier's
/// own light unloseable, and the reach filter must drop a carrier who
/// walked out of the window exactly as it drops a placed torch.
#[test]
fn a_carried_light_is_a_light_like_any_other() {
	let cam = [100.0, 64.0, 100.0];
	let reach = lateral_max(20);
	let class = digblock_core::gi::gi_class(digblock_core::chunk::make_cell(
		digblock_core::block::BlockType::Torch,
		0,
	));
	// A full list of placed torches, every one of them in reach and
	// none of them at the camera.
	let placed: Vec<_> = (0..MAX_LIGHTS)
		.map(|i| Emitter {
			pos: [cam[0] + 2.0 + i as f32 * 0.01, cam[1], cam[2]],
			class,
			level: 0,
			whitened: false,
			face: None,
		})
		.collect();
	let mine = digblock_core::gi::carried_origin(glam::Vec3::from_array(cam));
	let carried = [Emitter {
		pos: mine.to_array(),
		class,
		level: 0,
		whitened: false,
		face: None,
	}];
	let out = select_lights(placed.iter().copied(), &carried, cam, reach);
	assert_eq!(out.len(), MAX_LIGHTS, "the list still fills exactly once");
	let entry = out
		.iter()
		.find(|e| [e[0], e[1], e[2]] == mine.to_array())
		.expect(
			"the light in your own hand is the nearest thing there is — a \
				 full world must not be able to truncate it away",
		);
	assert_eq!(
		entry[3].to_bits() & 255,
		u32::from(class),
		"a carried torch carries the same class byte a placed one does, \
			 so tint and level come from one LUT entry"
	);
	assert_eq!(
		entry[3].to_bits() & LIGHT_CARRIED,
		LIGHT_CARRIED,
		"and the carried flag on top, which is the only thing that \
			 distinguishes it — see LIGHT_CARRIED for why the margin needs it"
	);
	// The placed ones must NOT wear it, or every torch in the world
	// stops stopping its own shadow ray short of its own cell.
	assert!(
		out.iter()
			.filter(|e| [e[0], e[1], e[2]] != mine.to_array())
			.all(|e| e[3].to_bits() & LIGHT_CARRIED == 0),
		"a placed torch is not a carried one"
	);
	// Out past the reach every march breaks at: dropped, like any
	// other emitter no ray could have traced.
	let far = [Emitter {
		pos: [cam[0] + reach + 1.0, cam[1], cam[2]],
		class,
		level: 0,
		whitened: false,
		face: None,
	}];
	assert!(select_lights(std::iter::empty(), &far, cam, reach).is_empty());
}

/// A per-entry level has to reach the shader ON TOP of the class and
/// the carried flag, and an entry without one has to pack exactly the
/// word it packed before the field existed — otherwise every placed
/// torch in the world quietly changes brightness.
#[test]
fn a_wielders_level_rides_the_entry_and_nothing_elses() {
	let cam = [0.0, 0.0, 0.0];
	let class = digblock_core::gi::gi_class(digblock_core::chunk::make_cell(
		digblock_core::block::BlockType::ResonantPick,
		0,
	));
	let held = |level, whitened| {
		let e = [Emitter {
			pos: [1.0, 0.0, 0.0],
			class,
			level,
			whitened,
			face: None,
		}];
		select_lights(std::iter::empty(), &e, cam, 8.0)[0][3].to_bits()
	};
	let plain = held(0, false);
	assert_eq!(
		plain,
		u32::from(class) | LIGHT_CARRIED,
		"no override, no bits"
	);
	let raised = held(13, true);
	assert_eq!(raised & 255, u32::from(class), "the class is untouched");
	assert_eq!(raised & LIGHT_CARRIED, LIGHT_CARRIED, "so is the flag");
	assert_eq!(
		(raised >> LIGHT_LEVEL_SHIFT) & LIGHT_LEVEL_MASK,
		13,
		"and the level reads back out of its own field"
	);
	assert_eq!(
		raised & LIGHT_WHITENED,
		LIGHT_WHITENED,
		"and the tint flag out of its own bit"
	);
	// The two per-holder properties are independent: a light may be
	// raised without being whitened, and the level field must not
	// smear into the flag above it.
	assert_eq!(held(LIGHT_LEVEL_MASK as u8, false) & LIGHT_WHITENED, 0);
}

/// The carried flag is packed here and unpacked in gi_dda.wgsl, which
/// both NEE passes take as a prelude. Nothing but agreement makes the
/// two halves one thing, so agreement is what gets pinned.
#[test]
fn the_carried_flag_matches_the_shaders() {
	for decl in [
		format!("const GI_LIGHT_CARRIED = {LIGHT_CARRIED}u;"),
		format!("const GI_LIGHT_LEVEL_SHIFT = {LIGHT_LEVEL_SHIFT}u;"),
		format!("const GI_LIGHT_LEVEL_MASK = {LIGHT_LEVEL_MASK}u;"),
		format!("const GI_LIGHT_WHITENED = {LIGHT_WHITENED}u;"),
		format!("const GI_LIGHT_QUAD = {LIGHT_QUAD}u;"),
	] {
		assert!(
			digblock_assets::shader::GI_DDA.contains(&decl),
			"gi_dda.wgsl must declare `{decl}`"
		);
	}
	// And it has to stay clear of the class byte it shares the word
	// with, which is what makes the mask legal at all. The other side
	// needs no assert: `Emitter`'s class is a u8.
	assert_eq!(LIGHT_CARRIED & 255, 0);
	// Five tenants in one word now, and the packing is only legal
	// while none of them overlaps another.
	let level = LIGHT_LEVEL_MASK << LIGHT_LEVEL_SHIFT;
	assert_eq!(level & (255 | LIGHT_CARRIED), 0);
	assert_eq!(LIGHT_WHITENED & (255 | LIGHT_CARRIED | level), 0);
	assert_eq!(
		LIGHT_QUAD & (255 | LIGHT_CARRIED | level | LIGHT_WHITENED),
		0
	);
}

/// Every estimator clamps the list's own header count to the buffer's
/// capacity, and that capacity is `MAX_LIGHTS` — the allocation
/// `flush_lights` selects into and truncates to. The shader cannot see
/// the Rust const, so raising one without the other silently discards
/// exactly the emitters the CPU took the trouble to keep: they sit in
/// the buffer, the header counts them, and every march walks past the
/// tail. This crate is the one that sees both sides.
///
/// The rule is checked as "no unclamped read" rather than against a
/// fixed number of loops, because R34 made a second estimator a second
/// copy of the clamp and there is nothing to stop a third.
#[test]
fn every_light_loop_clamps_to_the_lists_capacity() {
	let src = digblock_assets::shader::GI_COMMON;
	let clamp = "min(bitcast<u32>(lights[0].x), ";
	let mut clamped = 0;
	for tail in src.split(clamp).skip(1) {
		let cap = tail.split("u)").next().expect("the clamp must close");
		assert_eq!(
			cap.parse::<usize>().ok(),
			Some(MAX_LIGHTS),
			"a light loop clamps to {cap}, not MAX_LIGHTS"
		);
		clamped += 1;
	}
	assert!(clamped > 0, "gi_common.wgsl declares no clamped light loop");
	// A bare read is the failure this pin exists for, so the readers
	// are counted rather than assumed to be the estimators. Matched on
	// the bitcast the read needs, which the prose above them does not
	// spell out.
	assert_eq!(
		src.matches("bitcast<u32>(lights[0].x)").count(),
		clamped,
		"a light loop reads the header count without clamping it"
	);
}

/// The whole-chunk reach skip may only ever be CONSERVATIVE: it exists
/// to avoid touching entries, so admitting a chunk the per-emitter test
/// then rejects costs a little time, while dropping one that held a
/// reachable light silently puts a room out.
///
/// Swept against the per-emitter rule over every cell of a band of
/// chunks, which is the only way to catch the failure that matters —
/// the corner cells, where a chunk's nearest edge is much closer than
/// its centre.
#[test]
fn the_chunk_reach_skip_never_drops_a_reachable_light() {
	let cam = [37.5, 64.0, -12.25];
	let reach = lateral_max(20);
	let n = digblock_core::chunk::CHUNK_SIZE as i32;
	let mut admitted = 0;
	let mut skipped = 0;
	for cz in -14..14 {
		for cx in -14..14 {
			let pos = ChunkPos::new(cx, 4, cz);
			let ok = chunk_in_reach(pos, cam, reach);
			if ok {
				admitted += 1;
			} else {
				skipped += 1;
			}
			// Every cell of the chunk, corners included.
			for lz in 0..=n {
				for lx in 0..=n {
					let (x, z) = ((cx * n + lx) as f32, (cz * n + lz) as f32);
					let (dx, dz) = (x - cam[0], z - cam[2]);
					if dx * dx + dz * dz <= reach * reach {
						assert!(
							ok,
							"chunk {cx},{cz} skipped, but ({x},{z}) is inside the reach"
						);
					}
				}
			}
		}
	}
	// Anti-vacuity in both directions: a test where everything is
	// admitted proves nothing about the skip, and one where nothing is
	// proves nothing about the conservatism.
	assert!(admitted > 0 && skipped > 0, "{admitted} in, {skipped} out");
}

/// A rectangle reaches the shader as a PAIR — position and class word,
/// then extents and facing — and a point source's half of that pair
/// stays zero.
///
/// The stride is the thing to guard. Both NEE passes index
/// `lights[i * 2 ...]`, so an entry that packed one vec4 or three
/// would not fail loudly: every light past the first would simply be
/// read out of the middle of its neighbour, which looks like a
/// lighting bug rather than a format one.
#[test]
fn a_surface_emitter_packs_its_rectangle_beside_its_position() {
	let cam = [0.0, 0.0, 0.0];
	let class = digblock_core::gi::gi_class(BlockType::Emberore as u16);
	let seam = Emitter {
		pos: [2.0, 3.0, 4.0],
		class,
		level: 0,
		whitened: false,
		face: Some(Face {
			axis: 1,
			positive: true,
			ext: [3.0, 2.0],
		}),
	};
	let point = Emitter {
		pos: [1.0, 0.0, 0.0],
		class,
		level: 0,
		whitened: false,
		face: None,
	};
	let out = select_lights([seam, point].into_iter(), &[], cam, 32.0);
	assert_eq!(out.len(), 2, "one entry each, whatever their shape");
	let e = out
		.iter()
		.find(|e| [e[0], e[1], e[2]] == seam.pos)
		.expect("the seam");
	assert_eq!(
		e[3].to_bits() & LIGHT_QUAD,
		LIGHT_QUAD,
		"a rectangle has to announce itself, or it is read as a point"
	);
	assert_eq!(e[3].to_bits() & 255, u32::from(class), "class untouched");
	assert_eq!([e[4], e[5]], [3.0, 2.0], "extents, in the packer's order");
	// Facing: the axis in the low bits, the sign above it — the same
	// decode `gi_light_sample` does.
	let code = e[6].to_bits();
	assert_eq!(code & 3, 1, "axis");
	assert_eq!(code & 4, 4, "outward normal points up the axis");
	let p = out
		.iter()
		.find(|e| [e[0], e[1], e[2]] == point.pos)
		.expect("the point");
	assert_eq!(p[3].to_bits() & LIGHT_QUAD, 0, "a torch is not a rectangle");
	assert_eq!(
		[p[4], p[5], p[6], p[7]],
		[0.0; 4],
		"and its second half stays zero, so nothing reads a stale extent"
	);
}

/// The ceiling must sit at the TOP of the highest occupied brick (a
/// ceiling one brick low un-shadows every hilltop), and an empty
/// mirror must answer "nothing anywhere" rather than the world
/// ceiling (which would disable the early-out forever on an ocean
/// world).
#[test]
fn the_sky_ceiling_tops_the_highest_brick() {
	let (w, h, d) = (40usize, 32, 40);
	let mut mirror = vec![0u8; w * h * d];
	assert_eq!(sky_ceiling(&mirror, w, h, d), -1.0);
	mirror[(17 * h + 9) * w + 33] = 0b01;
	assert_eq!(sky_ceiling(&mirror, w, h, d), (9 * 8 + 7) as f32);
	// A higher EMITTER-only brick raises it too — the ceiling gates
	// hits of any kind.
	mirror[(2 * h + 24) * w + 5] = 0b10;
	assert_eq!(sky_ceiling(&mirror, w, h, d), (24 * 8 + 7) as f32);
}

/// The sweep must be a PERMUTATION, not merely a shuffle: over one
/// full cycle every probe has to be traced exactly once, or some
/// probes never converge and others burn budget twice. That is the
/// whole reason the stride has to be coprime with the count, and it
/// is the kind of property that fails silently — the picture just
/// keeps a stale patch somewhere.
#[test]
fn the_permuted_sweep_visits_every_probe_exactly_once() {
	for dims in [C0_DIMS, C1_DIMS, (1, 1, 1), (7, 5, 3), (16, 16, 16)] {
		let count = dims.0 * dims.1 * dims.2;
		let stride = schedule_stride(count);
		let mut seen = vec![false; count as usize];
		for k in 0..count {
			let pi = ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
			assert!(!seen[pi as usize], "{dims:?}: probe {pi} traced twice");
			seen[pi as usize] = true;
		}
		assert!(
			seen.iter().all(|&s| s),
			"{dims:?}: a probe was never traced"
		);
	}
}

/// `wg.x * stride` is computed in u32 IN THE SHADER, where an
/// overflow silently wraps and the permutation stops being one.
#[test]
fn the_stride_cannot_overflow_the_shaders_multiply() {
	for dims in [C0_DIMS, C1_DIMS, (64, 64, 64), (1, 1, 1)] {
		let count = dims.0 * dims.1 * dims.2;
		let stride = schedule_stride(count);
		assert!(
			u64::from(stride) * u64::from(PROBES_PER_DISPATCH) <= u64::from(u32::MAX),
			"{dims:?}: stride {stride} overflows"
		);
	}
}

/// The history format must be able to REPRESENT the increment its history
/// length asks for. Twice now it could not: rgba8 first, then rgba16f, both
/// of which let `hi + (x - hi)/N_MAX` round to zero and froze the running
/// average at whatever it held — permanently, per pixel, in a pattern locked
/// to wherever the player had walked. It took a capture harness that can see
/// motion to notice either time, because a still has already converged.
///
/// So the two knobs are pinned against each other: blending at alpha 1/N
/// stalls once the residual falls under about N ulps of the stored value.
/// Raising N_MAX or dropping the format now fails here instead.
#[test]
fn history_format_resolves_the_accumulator() {
	// N_MAX lives in the shader; read it rather than duplicating it,
	// so the two cannot drift.
	let src = digblock_assets::shader::GI_RESOLVE;
	let n_max: f32 = src
		.split("const N_MAX = ")
		.nth(1)
		.and_then(|s| s.split(';').next())
		.and_then(|s| s.trim().parse().ok())
		.expect("gi_resolve.wgsl must declare `const N_MAX = <f32>;`");

	// Explicit mantissa bits per format — the property under test, not
	// something to infer from a byte count.
	let mantissa_bits = match GI_ACC_FORMAT {
		wgpu::TextureFormat::Rgba32Float => 23,
		wgpu::TextureFormat::Rgba16Float => 10,
		wgpu::TextureFormat::Rgba8Unorm => 8,
		other => panic!("unknown accumulator format {other:?} — add its mantissa here"),
	};
	// Around a stored value of 1.0 (radiance and visibility both sit
	// there), an update rounds away once d/N < ulp/2.
	let stall = n_max * 2f32.powi(-(mantissa_bits + 1));
	let one_lsb = 1.0 / 255.0;
	assert!(
		stall < one_lsb,
		"{GI_ACC_FORMAT:?} with N_MAX {n_max} freezes the average while it is still \
			 {stall:.4} out (one 8-bit LSB is {one_lsb:.4}) — the running mean stops moving \
			 and every pixel keeps whatever it held when the camera stopped"
	);
}

/// Read a `const NAME = <number>;` out of a WGSL source (the pins
/// below hold shader constants against Rust ones, so they read the
/// shader rather than duplicating the number).
fn wgsl_f32(src: &str, name: &str) -> f32 {
	let pat = format!("const {name} = ");
	let at = src
		.find(&pat)
		.unwrap_or_else(|| panic!("{name}: not declared"));
	let rest = &src[at + pat.len()..];
	let end = rest
		.find(';')
		.unwrap_or_else(|| panic!("{name}: unterminated"));
	rest[..end]
		.trim()
		.parse()
		.unwrap_or_else(|e| panic!("{name}: not a number: {e}"))
}

/// The sun target's g channel packs `code + 256 * n`, and every reader
/// unpacks with integer bit-ops — which only works while the packed
/// integer is EXACT in the accumulator format. rgba32f holds integers
/// to 2^24; someone dropping the target to rgba16f "because the sun
/// history is short now" would corrupt the pack (and the indirect
/// counter inside it, which still climbs to N_MAX) long before the
/// average itself froze.
#[test]
fn packed_metadata_stays_exact() {
	let n_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_MAX");
	let mantissa_bits = match GI_ACC_FORMAT {
		wgpu::TextureFormat::Rgba32Float => 23,
		wgpu::TextureFormat::Rgba16Float => 10,
		wgpu::TextureFormat::Rgba8Unorm => 8,
		other => panic!("unknown accumulator format {other:?} — add its mantissa here"),
	};
	let packed_max = 255.0 + 256.0 * n_max;
	let exact_to = 2f32.powi(mantissa_bits + 1);
	assert!(
		packed_max < exact_to,
		"code + 256*N_MAX ({packed_max}) does not round-trip in {GI_ACC_FORMAT:?} \
			 (integers are exact only below {exact_to}) — the face code and history \
			 length come back corrupted"
	);
}

/// The resolve turns the occluder distance into a penumbra width with
/// SUN_TAN, which must be the tangent of the SAME cone the per-pixel
/// direction offset samples (SUN_CONE_COS) — otherwise the dithered
/// edge band and the reconstruction kernel disagree about how wide the
/// penumbra is, and one of them shows.
#[test]
fn the_penumbra_knows_the_cone() {
	let sun_tan = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "SUN_TAN");
	let want = SUN_CONE_COS.acos().tan();
	let rel = (sun_tan - want).abs() / want;
	assert!(
		rel < 0.01,
		"SUN_TAN ({sun_tan}) is not tan(acos(SUN_CONE_COS)) = {want} — the cone and \
			 the reconstruction width have drifted apart"
	);
}

/// The widest penumbra the resolve will store (PEN_MAX) must fit
/// inside what the filter can actually spread: R taps per side, per
/// iteration, at that iteration's stride, scaled by at most
/// SUN_STRIDE_MAX. Widening one constant without the other leaves
/// shadows that claim a width the kernel never delivers.
#[test]
fn the_penumbra_fits_the_filter() {
	let pen_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PEN_MAX");
	let stride_max = wgsl_f32(digblock_assets::shader::GI_ATROUS, "SUN_STRIDE_MAX");
	let reach: f32 = ATROUS_STEPS.iter().map(|s| s * stride_max).sum();
	assert!(
		pen_max <= reach,
		"PEN_MAX ({pen_max}) exceeds the filter's combined support ({reach} texels: \
			 strides {ATROUS_STEPS:?} x SUN_STRIDE_MAX {stride_max})"
	);
}

/// The disocclusion prior primes a fresh pixel's history from the
/// probe cache. It must stay under both caps that could otherwise be
/// asked to correct it: a primed pixel that is immediately moving
/// lives at N_MOVING, and the sun alpha rides min(n, N_SUN) — a prior
/// claiming more confidence than either would slow the very signals
/// the short caps exist to keep responsive.
#[test]
fn the_disocclusion_prior_stays_under_the_caps() {
	let n_prior = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_PRIOR");
	let n_moving = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_MOVING");
	let n_sun = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "N_SUN");
	assert!(n_prior > 1.0, "a prior of 1 is no prior at all");
	assert!(
		n_prior <= n_sun && n_prior < n_moving,
		"N_PRIOR ({n_prior}) must stay under N_SUN ({n_sun}) and N_MOVING ({n_moving})"
	);
	// And its two gates must exist as real gates: a margin that admits
	// nothing extra would mark every depth rejection as grazing (the
	// prime never fires), a sun ceiling that admits everything would
	// prime fully sun-lit pixels (which shows up as washed-out AO
	// across the walk and fly regimes).
	let margin = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PRIME_MARGIN");
	let sun_max = wgsl_f32(digblock_assets::shader::GI_RESOLVE, "PRIME_SUN_MAX");
	assert!(margin > 1.0, "PRIME_MARGIN ({margin}) gates nothing at ≤1");
	assert!(
		sun_max > 0.0 && sun_max < 1.0,
		"PRIME_SUN_MAX ({sun_max}) must partition lit from unlit"
	);
}

/// The LUT's packed sub-box words must decode in the shader to the
/// twelfths the shape authored — `gi_boxes_hit` unpacks the min
/// triple and then the max triple out of one word. The core side pins
/// the same format from the packer's end
/// (`box_pack_matches_the_shader_decode`); this end pins that the LUT
/// actually carries the boxes: every synthetic class entry is the
/// PARTIAL opacity class with a nonzero first box, borrowing its base
/// block's light level.
#[test]
fn the_lut_carries_the_shape_covers() {
	let lut = material_lut(&[]);
	let base = digblock_core::gi::SHAPE_CLASS_BASE as usize;
	let classes = digblock_core::gi::shape_classes();
	assert!(!classes.is_empty());
	for (i, sc) in classes.iter().enumerate() {
		let e = lut[base + i];
		assert_eq!(e[0] >> 24, 3, "synthetic class {i} is not partial");
		assert_ne!(e[2], 0, "synthetic class {i} lost its first box");
		assert_eq!(e[1] & 0xff, u32::from(sc.base.props().light));
	}
	// And the torch's emitting class is synthetic now — the light
	// list names classes, so the level has to ride along.
	let torch = digblock_core::gi::gi_class(BlockType::Torch as u16);
	assert_eq!(
		lut[torch as usize][1] & 0xff,
		u32::from(BlockType::Torch.props().light)
	);
}

/// Word 1 carries the cast COLOUR above the level, and for every row
/// but the ore it is the row's own albedo — so nothing that lit a room
/// before this field existed lights it differently now.
///
/// The ores are the exception the field was added for. Their textures
/// mean out to stone, so a derived tint would have every tier casting
/// the same colourless light; they state their fleck palette instead,
/// and that palette must actually reach the LUT.
#[test]
fn the_cast_colour_is_the_albedo_except_where_a_vein_says_otherwise() {
	// One distinct mean per layer, so "took the right one" is
	// checkable rather than vacuously true.
	let means: Vec<[u8; 3]> = (0..digblock_core::block::tex::COUNT)
		.map(|i| [i as u8, 200 - i as u8, 40 + i as u8])
		.collect();
	let lut = material_lut(&means);
	let tint = |b: BlockType| {
		let w = lut[b as usize][1] >> 8;
		[
			(w & 0xff) as u8,
			((w >> 8) & 0xff) as u8,
			((w >> 16) & 0xff) as u8,
		]
	};
	for b in [
		BlockType::Torch,
		BlockType::Lantern,
		BlockType::EssenceSiphon,
	] {
		assert_eq!(
			tint(b),
			means[b.props().tex_top as usize],
			"{b:?} must still cast its own texture's colour"
		);
	}
	for &ore in &digblock_core::worldgen::ore::ORES {
		let (core, _) = ore.node_flecks().expect("a tier");
		assert_eq!(tint(ore), core, "{ore:?} casts its flecks, not its rock");
		assert_ne!(
			tint(ore),
			means[ore.props().tex_top as usize],
			"{ore:?}: the whole point is that this is NOT the texture mean"
		);
		assert!(
			lut[ore as usize][1] & 0xff > 0,
			"{ore:?} has a colour and no light to cast in it"
		);
	}
}

/// A contiguous cursor is what made convergence sweep as a visible
/// band. Consecutive sweep steps must land far apart in the probe
/// grid — this is the property, not an implementation detail.
#[test]
fn consecutive_sweep_steps_are_not_neighbours() {
	let count = C0_DIMS.0 * C0_DIMS.1 * C0_DIMS.2;
	let stride = schedule_stride(count);
	let probe = |k: u32| ((u64::from(k) * u64::from(stride)) % u64::from(count)) as u32;
	let mut worst = u32::MAX;
	for k in 0..count.min(4096) {
		let d = probe(k).abs_diff(probe(k + 1));
		worst = worst.min(d.min(count - d));
	}
	assert!(
		worst > count / 8,
		"consecutive steps land {worst} apart out of {count} — that is a band"
	);
}
