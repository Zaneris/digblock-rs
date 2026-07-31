//! Compile-time embedded assets. Everything the game loads lives here so
//! macOS, iOS, and any future platform get byte-identical assets with zero
//! bundle-path logic.

pub mod tex {
	pub const WATER: &[u8] = include_bytes!("../../../assets/textures/Water.png");
	pub const DIRT: &[u8] = include_bytes!("../../../assets/textures/Dirt.png");
	pub const GRASS_SIDE: &[u8] = include_bytes!("../../../assets/textures/GrassSide.png");
	pub const GRASS_TOP: &[u8] = include_bytes!("../../../assets/textures/GrassTop.png");

	/// Texture-array layer order. Must match the Java texture indices
	/// (Block.getTextureFromBlockType): 0=Water 1=Dirt 2=GrassSide 3=GrassTop.
	pub const LAYERS: [&[u8]; 4] = [WATER, DIRT, GRASS_SIDE, GRASS_TOP];
}

pub mod shader {
	pub const CHUNK: &str = include_str!("../../../assets/shaders/chunk.wgsl");
	pub const SHADOW: &str = include_str!("../../../assets/shaders/shadow.wgsl");
	pub const OVERLAY: &str = include_str!("../../../assets/shaders/overlay.wgsl");

	pub const ALL: [(&str, &str); 3] = [
		("chunk.wgsl", CHUNK),
		("shadow.wgsl", SHADOW),
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
