//! Block texture array + sampler (the materials bind group's contents).
//! Full mip chain generated on CPU (box filter), sampled
//! min=linear/mip=linear, mag=nearest — the Java
//! MipMapLinearLinear/Nearest look.

use digblock_platform::Gpu;

pub struct BlockTextures {
	pub array_view: wgpu::TextureView,
	pub sampler: wgpu::Sampler,
	/// Mean rgb per texture layer (gamma-space, like the atlas) — the
	/// GI material LUT's albedo source (GI-DESIGN.md): rays never
	/// sample the atlas, they read one color per class.
	pub layer_means: Vec<[u8; 3]>,
}

fn mip_levels(size: u32) -> u32 {
	32 - size.leading_zeros()
}

fn box_downsample(src: &[u8], w: usize, h: usize) -> Vec<u8> {
	let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
	let mut out = vec![0u8; nw * nh * 4];
	for y in 0..nh {
		for x in 0..nw {
			for c in 0..4 {
				let mut sum = 0u32;
				for (sy, sx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
					let px = (x * 2 + sx).min(w - 1);
					let py = (y * 2 + sy).min(h - 1);
					sum += u32::from(src[(py * w + px) * 4 + c]);
				}
				out[(y * nw + x) * 4 + c] = (sum / 4) as u8;
			}
		}
	}
	out
}

impl BlockTextures {
	pub fn new(gpu: &Gpu) -> Self {
		const SIZE: u32 = 64;
		let levels = mip_levels(SIZE);
		let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("block array"),
			size: wgpu::Extent3d {
				width: SIZE,
				height: SIZE,
				depth_or_array_layers: digblock_assets::tex::LAYERS.len() as u32,
			},
			mip_level_count: levels,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			format: wgpu::TextureFormat::Rgba8Unorm, // gamma-space on purpose
			usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
			view_formats: &[],
		});

		let mut layer_means = Vec::with_capacity(digblock_assets::tex::LAYERS.len());
		for (layer, bytes) in digblock_assets::tex::LAYERS.iter().enumerate() {
			let img = image::load_from_memory(bytes)
				.expect("decode block texture")
				.to_rgba8();
			layer_means.push(mean_rgb(&img));
			assert_eq!(
				(img.width(), img.height()),
				(SIZE, SIZE),
				"block texture must be 64x64"
			);
			let mut level_pixels = img.into_raw();
			let (mut w, mut h) = (SIZE as usize, SIZE as usize);
			for level in 0..levels {
				gpu.queue.write_texture(
					wgpu::TexelCopyTextureInfo {
						texture: &texture,
						mip_level: level,
						origin: wgpu::Origin3d {
							x: 0,
							y: 0,
							z: layer as u32,
						},
						aspect: wgpu::TextureAspect::All,
					},
					&level_pixels,
					wgpu::TexelCopyBufferLayout {
						offset: 0,
						bytes_per_row: Some(4 * w as u32),
						rows_per_image: Some(h as u32),
					},
					wgpu::Extent3d {
						width: w as u32,
						height: h as u32,
						depth_or_array_layers: 1,
					},
				);
				if level + 1 < levels {
					level_pixels = box_downsample(&level_pixels, w, h);
					w = (w / 2).max(1);
					h = (h / 2).max(1);
				}
			}
		}

		let array_view = texture.create_view(&wgpu::TextureViewDescriptor {
			dimension: Some(wgpu::TextureViewDimension::D2Array),
			..Default::default()
		});

		let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("block sampler"),
			address_mode_u: wgpu::AddressMode::ClampToEdge,
			address_mode_v: wgpu::AddressMode::ClampToEdge,
			address_mode_w: wgpu::AddressMode::ClampToEdge,
			mag_filter: wgpu::FilterMode::Nearest,
			min_filter: wgpu::FilterMode::Linear,
			mipmap_filter: wgpu::FilterMode::Linear,
			..Default::default()
		});

		Self {
			array_view,
			sampler,
			layer_means,
		}
	}
}

/// Alpha-weighted mean color (cutout textures like leaves must not
/// average toward their transparent filler).
fn mean_rgb(img: &image::RgbaImage) -> [u8; 3] {
	let (mut sum, mut weight) = ([0u64; 3], 0u64);
	for p in img.pixels() {
		let a = u64::from(p[3]);
		for c in 0..3 {
			sum[c] += u64::from(p[c]) * a;
		}
		weight += a;
	}
	if weight == 0 {
		return [0; 3];
	}
	[
		(sum[0] / weight) as u8,
		(sum[1] / weight) as u8,
		(sum[2] / weight) as u8,
	]
}

// The texture array must fit the packed vertex format's layer field —
// caught here at compile time, not as GPU corruption on the first layer
// past the field's range.
const _: () = assert!(digblock_assets::tex::LAYERS.len() <= digblock_core::block::MAX_TEX_LAYERS);

// And the two lists that have to agree, agree. `digblock-core` names the
// layers by index and `digblock-assets` supplies them in that order, and
// neither crate can see the other — this file is the only place both are
// in scope. A length skew is what an inserted or deleted PNG looks like,
// and it silently textures every block after it with its neighbour's
// image, so it fails the BUILD rather than a test.
const _: () =
	assert!(digblock_assets::tex::LAYERS.len() == digblock_core::block::tex::COUNT as usize);

#[cfg(test)]
mod layer_tests {
	/// Every PROPS texture index must name an existing LAYERS entry —
	/// the cross-crate half of the add-a-block skew check.
	#[test]
	fn props_reference_existing_layers() {
		let layers = digblock_assets::tex::LAYERS.len() as u16;
		for &b in &digblock_core::block::BlockType::ALL {
			let p = b.props();
			for t in [p.tex_side, p.tex_top, p.tex_bottom] {
				assert!(t < layers, "{b:?} references missing layer {t}");
			}
		}
	}
}
