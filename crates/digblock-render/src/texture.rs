//! Block texture array + samplers + placeholder shadow maps.
//! 4 layers of 64² (Water/Dirt/GrassSide/GrassTop), full mip chain
//! generated on CPU (box filter), sampled min=linear/mip=linear,
//! mag=nearest — the Java MipMapLinearLinear/Nearest look.

use digblock_platform::Gpu;

pub struct BlockTextures {
	pub array_view: wgpu::TextureView,
	pub sampler: wgpu::Sampler,
	pub shadow_near_view: wgpu::TextureView,
	/// Two wide maps, ping-ponged: one is sampled while the other is
	/// re-rendered a slice per frame (the refresh would otherwise encode
	/// every resident chunk in one frame and blow the vsync budget).
	pub shadow_wide_views: [wgpu::TextureView; 2],
	pub shadow_sampler: wgpu::Sampler,
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
	pub fn new(gpu: &Gpu, shadow_size: u32) -> Self {
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

		for (layer, bytes) in digblock_assets::tex::LAYERS.iter().enumerate() {
			let img = image::load_from_memory(bytes)
				.expect("decode block texture")
				.to_rgba8();
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

		// Shadow maps: real render targets from day one (M6 renders into
		// them; until then they clear to depth 1.0 = fully lit).
		let make_shadow = |label: &str| {
			gpu.device
				.create_texture(&wgpu::TextureDescriptor {
					label: Some(label),
					size: wgpu::Extent3d {
						width: shadow_size,
						height: shadow_size,
						depth_or_array_layers: 1,
					},
					mip_level_count: 1,
					sample_count: 1,
					dimension: wgpu::TextureDimension::D2,
					format: wgpu::TextureFormat::Depth32Float,
					usage: wgpu::TextureUsages::RENDER_ATTACHMENT
						| wgpu::TextureUsages::TEXTURE_BINDING,
					view_formats: &[],
				})
				.create_view(&wgpu::TextureViewDescriptor::default())
		};
		let shadow_near_view = make_shadow("shadow near");
		let shadow_wide_views = [make_shadow("shadow wide A"), make_shadow("shadow wide B")];

		let shadow_sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("shadow comparison"),
			address_mode_u: wgpu::AddressMode::ClampToEdge,
			address_mode_v: wgpu::AddressMode::ClampToEdge,
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			compare: Some(wgpu::CompareFunction::LessEqual),
			..Default::default()
		});

		Self {
			array_view,
			sampler,
			shadow_near_view,
			shadow_wide_views,
			shadow_sampler,
		}
	}
}
