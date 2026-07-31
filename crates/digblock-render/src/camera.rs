//! Debug fly camera (M4–M7). Replaced as the view source by the Player in
//! M8; kept behind a toggle afterwards.

use glam::{Mat4, Vec3};

pub struct FlyCamera {
	pub pos: Vec3,
	pub yaw_deg: f32,
	pub pitch_deg: f32,
	pub fov_y_deg: f32,
	pub near: f32,
	pub far: f32,
}

impl FlyCamera {
	pub fn new(far: f32, spawn: Vec3) -> Self {
		// Java defaults: looking +z, FOV 75°, near 0.1.
		Self {
			pos: spawn,
			yaw_deg: 0.0,
			pitch_deg: -20.0,
			fov_y_deg: 75.0,
			near: 0.1,
			far,
		}
	}

	pub fn dir(&self) -> Vec3 {
		let (ys, yc) = self.yaw_deg.to_radians().sin_cos();
		let (ps, pc) = self.pitch_deg.to_radians().sin_cos();
		// yaw 0 => +z (Java initial direction), yaw increases turning right.
		Vec3::new(ys * pc, ps, yc * pc).normalize()
	}

	pub fn look(&mut self, dx: f32, dy: f32) {
		self.yaw_deg -= dx;
		self.pitch_deg = (self.pitch_deg - dy).clamp(-89.0, 89.0);
	}

	pub fn view_proj(&self, aspect: f32) -> Mat4 {
		let proj = Mat4::perspective_rh(self.fov_y_deg.to_radians(), aspect, self.near, self.far);
		let view = Mat4::look_to_rh(self.pos, self.dir(), Vec3::Y);
		proj * view
	}
}
