//! Assembling a mesh job's input: a padded copy with a one-block shell
//! from every neighbour, plus the node stamp. The stamp writes block
//! IDS, not texture layers, so the mesher sees a real node block.

use super::*;

impl Pipeline {
	pub(super) fn build_mesh_input(&self, pos: ChunkPos, center: &ChunkData) -> MeshInput {
		let mut input = MeshInput::build(center, |dx, dy, dz| {
			let np = ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
			if np.y >= WORLD_VCHUNK {
				return Src::Air; // Java air sentinel above the world
			}
			if np.y < 0 {
				return Src::Missing; // Java null below
			}
			match self.store.get(&np) {
				Some(ChunkSlot {
					state: ChunkState::Ready(d),
					..
				}) => {
					if is_air_chunk(d) {
						Src::Air // skip the shell copy
					} else {
						Src::Chunk(d)
					}
				}
				_ => Src::Missing,
			}
		});
		input.nodes = self.node_stamp(pos, &input.blocks);
		input
	}

	/// The node reveal's per-job parcel, or `None` when this chunk cannot show a node at all. The
	/// edited-cell list is gathered HERE because this is the only place holding the map, and runs
	/// on the MAIN thread out of the streaming budget — hence both guards: no node source reaching
	/// the box means no stamp, and the edited-cell walk takes whichever side is smaller. The reveal
	/// LEVELS are NOT a guard; the host scan is, since a surface node reveals at the start level.
	pub(super) fn node_stamp(
		&self,
		pos: ChunkPos,
		blocks: &[u16; crate::mesher::PADDED_VOLUME],
	) -> Option<crate::mesher::NodeStamp> {
		use crate::worldgen::ore;
		let n = crate::chunk::CHUNK_SIZE as i32;
		let origin = (pos.x * n - 1, pos.y * n - 1, pos.z * n - 1);
		let span = crate::mesher::PADDED as i32 - 1;
		let far = (origin.0 + span, origin.1 + span, origin.2 + span);
		// Which hosts could matter HERE — the levels and the depth, asked
		// before the scan so the scan only has to recognize ids that are
		// still in play. Deposits are the one half a level test can still
		// skip outright, and the depth test beside it is the half that
		// really pays: most of a world is nowhere near the bands.
		let reveal_met = |b| crate::worldgen::nodes::reveal_met(b, self.levels);
		let want_deposits = origin.1 <= ore::MAX_DEPOSIT_Y + ore::MAX_RADIUS
			&& (self.levels.mining >= ore::lowest_reveal()
				|| self.levels.alchemy >= ore::lowest_alchemy_reveal());
		let want_heartwood = reveal_met(crate::block::BlockType::Heartwood);
		let want_springs = reveal_met(crate::block::BlockType::Springstone);
		if !want_deposits && !want_heartwood && !want_springs {
			return None;
		}
		let (mut stone, mut wood, mut sand) = (false, false, false);
		for &cell in blocks.iter() {
			// Only the hosts still in play are worth recognizing.
			match cell & crate::chunk::ID_MASK {
				id if want_deposits && id == crate::block::BlockType::Stone as u16 => stone = true,
				id if want_heartwood && id == crate::block::BlockType::Wood as u16 => wood = true,
				id if want_springs && id == crate::block::BlockType::Sand as u16 => sand = true,
				_ => {}
			}
		}
		// `deposits_near` is the expensive half and asks worldgen, so it
		// waits until a host has actually been seen.
		let hosts = crate::mesher::NodeHosts {
			deposits: stone && !ore::deposits_near(origin, far, self.seed).is_empty(),
			heartwood: wood,
			springs: sand,
		};
		if hosts.is_empty() {
			return None;
		}
		let mut cols = Vec::new();
		let mut entries = 0usize;
		for cz in origin.2.div_euclid(n)..=(origin.2 + span).div_euclid(n) {
			for cx in origin.0.div_euclid(n)..=(origin.0 + span).div_euclid(n) {
				if let Some(cells) = self.edits.column(cx, cz) {
					entries += cells.len();
					cols.push(((cx, cz), cells));
				}
			}
		}
		let volume = ((span + 1) as usize).pow(3);
		let mut edited = Vec::new();
		if entries <= volume {
			for (_, cells) in &cols {
				for &(x, y, z) in cells.keys() {
					let (lx, ly, lz) = (x - origin.0, y - origin.1, z - origin.2);
					if (0..=span).contains(&lx)
						&& (0..=span).contains(&ly)
						&& (0..=span).contains(&lz)
					{
						edited.push(crate::mesher::pidx(lx - 1, ly - 1, lz - 1) as u16);
					}
				}
			}
		} else {
			// One lookup per cell of the box, and each cell belongs to
			// exactly one column — so this is `volume` probes in total, not
			// per column.
			for ((cx, cz), cells) in &cols {
				let x0 = (cx * n).max(origin.0);
				let x1 = (cx * n + n - 1).min(origin.0 + span);
				let z0 = (cz * n).max(origin.2);
				let z1 = (cz * n + n - 1).min(origin.2 + span);
				for z in z0..=z1 {
					for x in x0..=x1 {
						for y in origin.1..=origin.1 + span {
							if cells.contains_key(&(x, y, z)) {
								edited.push(crate::mesher::pidx(
									x - origin.0 - 1,
									y - origin.1 - 1,
									z - origin.2 - 1,
								) as u16);
							}
						}
					}
				}
			}
		}
		edited.sort_unstable();
		Some(crate::mesher::NodeStamp {
			seed: self.seed,
			origin,
			levels: self.levels,
			hosts,
			edited,
		})
	}
}
