use super::*;
use digblock_core::worldgen::Seed;
use digblock_core::worldgen::ore;

/// The chamber's own radius, read from the plan rather than mirrored:
/// a copy that drifted would have this test checking a box the plan
/// never carved.
use super::debug_plans::ORE_CHAMBER_R as R;

/// DIG_ORE's one rule: the plan must not write over a single cell the deposit claims. The plan is
/// applied through `pipeline.set_cell`, which records into the `EditOverlay`, and `nodes::revealed`
/// answers None for an edited cell forever — so a stray write deletes the subject of the photograph
/// permanently and silently, because the chamber still looks right. `crossings` is what stops this
/// going vacuous: it counts the vein cells lying IN the floor plate's plane.
#[test]
fn the_ore_capture_plan_never_writes_over_the_vein() {
	let mut crossings = 0usize;
	let mut plans = 0usize;
	for i in 0..20 {
		let seed = Seed::current(1000.0 + f64::from(i) * 37.5);
		// Any pose does: the plan only reads spawn's x/z, to pick the
		// nearest deposit of the tier. Skipping the worldgen spawn
		// search keeps this a pure ore-geometry test.
		let spawn = Vec3::new(0.0, 90.0, 0.0);
		for &want in ore::ORES.iter().chain([BlockType::Leystone].iter()) {
			let (pos, plan) = debug_ore_plan(spawn, seed, want);
			if plan.is_empty() {
				continue;
			}
			plans += 1;
			for &(x, y, z, cell) in &plan {
				assert!(
					ore::ore_at(x, y, z, seed.value).is_none(),
					"seed {seed:?} tier {want:?}: plan writes {cell} over the vein \
						 at {x},{y},{z}"
				);
			}
			// The plate is the plan's only Stone, and the pose is
			// authored from the deposit centre — enough to rebuild the
			// footprint the unguarded loop used to pave.
			let plate: Vec<_> = plan
				.iter()
				.filter(|c| c.3 == BlockType::Stone as u16)
				.collect();
			let floor = plate[0].1;
			assert!(
				plate.iter().all(|c| c.1 == floor),
				"seed {seed:?} tier {want:?}: the plate is one plane"
			);
			let (cx, cz) = ((pos.x - 0.5) as i32, (pos.z + 4.5) as i32);
			let mut vein = 0usize;
			for z in (cz - R)..=(cz + R) {
				for x in (cx - R)..=(cx + R) {
					vein += usize::from(ore::ore_at(x, floor, z, seed.value).is_some());
				}
			}
			let square = (2 * R + 1).pow(2) as usize;
			assert_eq!(
				plate.len() + vein,
				square,
				"seed {seed:?} tier {want:?}: the plate is the plane minus the vein"
			);
			crossings += vein;
		}
	}
	assert!(plans > 50, "the sample found deposits to photograph");
	assert!(
		crossings > 0,
		"no vein crossed a floor plane in {plans} plans — this sample \
			 cannot tell a guarded plate from an unguarded one"
	);
}

/// **The deposit the scene picks is one the stamp can actually reveal.** A vein manifests only in
/// pristine STONE, so a deposit rolled into open air — or under a lake — reveals nothing at any
/// level, and DIG_ORE photographs an empty stone-plated box that reads as a bug in the reveal.
/// `the_ore_capture_plan_never_writes_over_the_vein` does not catch it: sparing the vein's cells is
/// just as true when there is no rock around them, and each band's FLOOR lifts shallow tiers.
#[test]
fn every_capture_deposit_stands_in_rock() {
	let mut checked = 0usize;
	for i in 0..20 {
		let seed = Seed::current(1000.0 + f64::from(i) * 37.5);
		let spawn = Vec3::new(0.0, 90.0, 0.0);
		for &want in ore::ORES.iter().chain([BlockType::Leystone].iter()) {
			let (pos, plan) = debug_ore_plan(spawn, seed, want);
			if plan.is_empty() {
				continue;
			}
			// The pose is authored off the deposit centre, so the
			// centre column is recoverable from it — and the centre is
			// in the blob by construction.
			let (cx, cz) = ((pos.x - 0.5) as i32, (pos.z + 4.5) as i32);
			let col = digblock_core::worldgen::column_shape(cx, cz, seed);
			let cy = (0..digblock_core::chunk::WORLD_VBLOCK)
				.find(|&y| ore::ore_at(cx, y, cz, seed.value) == Some(want))
				.unwrap_or_else(|| panic!("seed {seed:?} {want:?}: no vein under the pose"));
			assert!(
				cy < col.height,
				"seed {seed:?} {want:?}: the scene stands at y {cy} under a surface \
					 at {} — the vein is in open air and the stamp will never fire",
				col.height
			);
			checked += 1;
		}
	}
	assert!(checked > 50, "the sample found deposits to photograph");
}
