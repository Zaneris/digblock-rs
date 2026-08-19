//! Debug fly camera (M4–M7). Replaced as the view source by the Player in
//! M8; kept behind a toggle afterwards.

// glam names the [0, 1] clip-depth convention after DirectX, and its own
// docs name WebGPU alongside it — that range is what this backend wants.
// `rh` is the −z-forward view space every shader here is written against.
use glam::camera::rh::proj::directx::perspective;
use glam::camera::rh::view::look_to_mat4;
use glam::{Mat4, Vec3};

pub(crate) struct FlyCamera {
	pub pos: Vec3,
	pub yaw_deg: f32,
	pub pitch_deg: f32,
	pub fov_y_deg: f32,
	pub near: f32,
	pub far: f32,
}

impl FlyCamera {
	pub(crate) fn new(far: f32, spawn: Vec3) -> Self {
		// Java defaults: looking +z, at Java's field of view and near plane.
		Self {
			pos: spawn,
			yaw_deg: 0.0,
			pitch_deg: -20.0,
			fov_y_deg: 75.0,
			near: 0.1,
			far,
		}
	}

	pub(crate) fn dir(&self) -> Vec3 {
		let (ys, yc) = self.yaw_deg.to_radians().sin_cos();
		let (ps, pc) = self.pitch_deg.to_radians().sin_cos();
		// yaw 0 => +z (Java initial direction), yaw increases turning right.
		Vec3::new(ys * pc, ps, yc * pc).normalize()
	}

	pub(crate) fn look(&mut self, dx: f32, dy: f32) {
		self.yaw_deg -= dx;
		self.pitch_deg = (self.pitch_deg - dy).clamp(-89.0, 89.0);
	}

	/// World → view space (+x right, +y up, −z forward). Split out of
	/// [`Self::view_proj`] for the viewmodel, which is authored in view
	/// space and needs the world's light direction brought into it.
	pub(crate) fn view(&self) -> Mat4 {
		look_to_mat4(self.pos, self.dir(), Vec3::Y)
	}

	pub(crate) fn view_proj(&self, aspect: f32) -> Mat4 {
		let proj = perspective(self.fov_y_deg.to_radians(), aspect, self.near, self.far);
		proj * self.view()
	}
}
