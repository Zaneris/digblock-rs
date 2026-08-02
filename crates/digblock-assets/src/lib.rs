//! Compile-time embedded assets. Everything the game loads lives here so
//! macOS, iOS, and any future platform get byte-identical assets with zero
//! bundle-path logic.

pub mod tex {
	pub const WATER: &[u8] = include_bytes!("../../../assets/textures/Water.png");
	pub const DIRT: &[u8] = include_bytes!("../../../assets/textures/Dirt.png");
	pub const GRASS_SIDE: &[u8] = include_bytes!("../../../assets/textures/GrassSide.png");
	pub const GRASS_TOP: &[u8] = include_bytes!("../../../assets/textures/GrassTop.png");
	pub const SAND: &[u8] = include_bytes!("../../../assets/textures/Sand.png");
	pub const STONE: &[u8] = include_bytes!("../../../assets/textures/Stone.png");
	pub const WOOD: &[u8] = include_bytes!("../../../assets/textures/Wood.png");
	pub const WOOD_TOP: &[u8] = include_bytes!("../../../assets/textures/WoodTop.png");
	pub const LEAVES: &[u8] = include_bytes!("../../../assets/textures/Leaves.png");
	pub const SNOW: &[u8] = include_bytes!("../../../assets/textures/Snow.png");

	/// Texture-array layer order — the layer index is what block PROPS
	/// reference (digblock-core block.rs `tex` module mirrors this list).
	/// Append only; never reorder.
	pub const LAYERS: [&[u8]; 10] = [
		WATER, DIRT, GRASS_SIDE, GRASS_TOP, SAND, STONE, WOOD, WOOD_TOP, LEAVES, SNOW,
	];
}

pub mod shader {
	// WGSL has no include mechanism, so shared preludes are prepended
	// with concat! at include time: frame_common.wgsl (the Frame uniform
	// block + its group(0) binding, one definition for every pass that
	// reads it) and sky_common.wgsl (the procedural-sky core — gradient,
	// glow, stars — shared so the sky pass and water reflections can
	// never silently drift apart). Prelude order is frame_common first,
	// then sky_common, then the body file; naga error line numbers in
	// the body files are offset by the total prelude length.
	pub const CHUNK: &str = concat!(
		include_str!("../../../assets/shaders/frame_common.wgsl"),
		include_str!("../../../assets/shaders/chunk.wgsl"),
	);
	pub const WATER: &str = concat!(
		include_str!("../../../assets/shaders/frame_common.wgsl"),
		include_str!("../../../assets/shaders/sky_common.wgsl"),
		include_str!("../../../assets/shaders/water.wgsl"),
	);
	pub const SHADOW: &str = include_str!("../../../assets/shaders/shadow.wgsl");
	pub const SKY: &str = concat!(
		include_str!("../../../assets/shaders/frame_common.wgsl"),
		include_str!("../../../assets/shaders/sky_common.wgsl"),
		include_str!("../../../assets/shaders/sky.wgsl"),
	);
	pub const OVERLAY: &str = include_str!("../../../assets/shaders/overlay.wgsl");
	pub const BLIT: &str = include_str!("../../../assets/shaders/blit.wgsl");
	pub const SPRAY: &str = concat!(
		include_str!("../../../assets/shaders/frame_common.wgsl"),
		include_str!("../../../assets/shaders/spray.wgsl"),
	);
	pub const PLAYERS: &str = concat!(
		include_str!("../../../assets/shaders/frame_common.wgsl"),
		include_str!("../../../assets/shaders/players.wgsl"),
	);

	pub const ALL: [(&str, &str); 8] = [
		("chunk.wgsl", CHUNK),
		("water.wgsl", WATER),
		("shadow.wgsl", SHADOW),
		("sky.wgsl", SKY),
		("overlay.wgsl", OVERLAY),
		("blit.wgsl", BLIT),
		("spray.wgsl", SPRAY),
		("players.wgsl", PLAYERS),
	];
}

#[cfg(test)]
mod tests {
	/// Validate every WGSL shader without needing a GPU, so CI catches
	/// shader errors on any runner.
	#[test]
	fn shaders_parse_and_validate() {
		for (name, src) in super::shader::ALL {
			let module = naga::front::wgsl::parse_str(src)
				.unwrap_or_else(|e| panic!("{name}: parse error: {e}"));
			naga::valid::Validator::new(
				naga::valid::ValidationFlags::all(),
				naga::valid::Capabilities::all(),
			)
			.validate(&module)
			.unwrap_or_else(|e| panic!("{name}: validation error: {e:?}"));
		}
	}

	#[test]
	fn textures_are_nonempty_pngs() {
		for bytes in super::tex::LAYERS {
			assert!(bytes.len() > 8);
			assert_eq!(&bytes[1..4], b"PNG");
		}
	}
}
