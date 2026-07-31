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
	pub const CHUNK: &str = include_str!("../../../assets/shaders/chunk.wgsl");
	pub const SHADOW: &str = include_str!("../../../assets/shaders/shadow.wgsl");
	pub const SKY: &str = include_str!("../../../assets/shaders/sky.wgsl");
	pub const OVERLAY: &str = include_str!("../../../assets/shaders/overlay.wgsl");

	pub const ALL: [(&str, &str); 4] = [
		("chunk.wgsl", CHUNK),
		("shadow.wgsl", SHADOW),
		("sky.wgsl", SKY),
		("overlay.wgsl", OVERLAY),
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
