//! Copies the just-rendered surface texture to a PNG. Requires the surface
//! to be configured with COPY_SRC (we always do). Runs synchronously — it's
//! a debug/CI tool, a hitch is fine.

use crate::Gpu;
use std::path::Path;

pub fn capture(gpu: &Gpu, texture: &wgpu::Texture, format: wgpu::TextureFormat, path: &Path) {
	let width = texture.width();
	let height = texture.height();
	let bytes_per_pixel = 4u32;
	let unpadded = width * bytes_per_pixel;
	let padded =
		unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

	let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
		label: Some("screenshot"),
		size: u64::from(padded) * u64::from(height),
		usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
		mapped_at_creation: false,
	});

	let mut encoder = gpu
		.device
		.create_command_encoder(&wgpu::CommandEncoderDescriptor {
			label: Some("screenshot"),
		});
	encoder.copy_texture_to_buffer(
		wgpu::TexelCopyTextureInfo {
			texture,
			mip_level: 0,
			origin: wgpu::Origin3d::ZERO,
			aspect: wgpu::TextureAspect::All,
		},
		wgpu::TexelCopyBufferInfo {
			buffer: &buffer,
			layout: wgpu::TexelCopyBufferLayout {
				offset: 0,
				bytes_per_row: Some(padded),
				rows_per_image: Some(height),
			},
		},
		wgpu::Extent3d {
			width,
			height,
			depth_or_array_layers: 1,
		},
	);
	gpu.queue.submit([encoder.finish()]);

	let slice = buffer.slice(..);
	slice.map_async(wgpu::MapMode::Read, |r| r.expect("map screenshot buffer"));
	let _ = gpu.device.poll(wgpu::PollType::Wait);

	let data = slice.get_mapped_range();
	let mut pixels = Vec::with_capacity((width * height * 4) as usize);
	let bgra = matches!(
		format,
		wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
	);
	for row in 0..height {
		let start = (row * padded) as usize;
		let row_bytes = &data[start..start + unpadded as usize];
		for px in row_bytes.chunks_exact(4) {
			if bgra {
				pixels.extend_from_slice(&[px[2], px[1], px[0], 255]);
			} else {
				pixels.extend_from_slice(&[px[0], px[1], px[2], 255]);
			}
		}
	}
	drop(data);
	buffer.unmap();

	if let Some(parent) = path.parent() {
		let _ = std::fs::create_dir_all(parent);
	}
	match image::RgbaImage::from_raw(width, height, pixels)
		.expect("screenshot buffer size")
		.save(path)
	{
		Ok(()) => log::info!("screenshot saved: {}", path.display()),
		Err(e) => log::error!("screenshot save failed: {e}"),
	}
}
