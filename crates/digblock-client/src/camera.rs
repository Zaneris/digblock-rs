//! Debug fly camera (M4–M7). Replaced as the view source by the Player in
//! M8; kept behind a toggle afterwards.

// glam names the [0, 1] clip-depth convention after DirectX, and its own
// docs name WebGPU alongside it — that range is what this backend wants.
// `rh` is the −z-forward view space every shader here is written against.
use glam::camera::rh::proj::directx::perspective;
use glam::camera::rh::view::look_to_mat4;
use glam::{Mat4, Vec3};

/// Facing angles to a unit vector: yaw 0 => +z (Java initial
/// direction); +yaw turns LEFT in the right-handed view, which is why
/// `look` subtracts the mouse's dx.
pub(crate) fn dir_from(yaw_deg: f32, pitch_deg: f32) -> Vec3 {
	let (ys, yc) = yaw_deg.to_radians().sin_cos();
	let (ps, pc) = pitch_deg.to_radians().sin_cos();
	Vec3::new(ys * pc, ps, yc * pc).normalize()
}

/// The player's FACING, owned by input. First person copies these into
/// the camera verbatim every frame; a detached camera leaves them alone
/// — so persistence, the wire, and movement always speak the player's
/// facing, never a rig's.
pub(crate) struct LookAngles {
	pub yaw_deg: f32,
	pub pitch_deg: f32,
}

impl LookAngles {
	/// Apply a look delta — the same convention as [`FlyCamera::look`].
	pub(crate) fn look(&mut self, dx: f32, dy: f32) {
		self.yaw_deg -= dx;
		self.pitch_deg = (self.pitch_deg - dy).clamp(-89.0, 89.0);
	}
}

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
		dir_from(self.yaw_deg, self.pitch_deg)
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

	/// The view-projection with this frame's [`CamOffset`] applied — the
	/// camera-feel seam. The idle offset takes the PLAIN path, bitwise,
	/// so a spent shake cannot leave a differently-rounded matrix.
	pub(crate) fn view_proj_with(&self, aspect: f32, fx: &crate::camera_fx::CamOffset) -> Mat4 {
		if *fx == crate::camera_fx::CamOffset::IDLE {
			return self.view_proj(aspect);
		}
		let dir = dir_from(
			self.yaw_deg,
			(self.pitch_deg + fx.pitch_deg).clamp(-89.9, 89.9),
		);
		let up = glam::Quat::from_axis_angle(dir, fx.roll_deg.to_radians()) * Vec3::Y;
		let view = look_to_mat4(self.pos + fx.pos, dir, up);
		let proj = perspective(
			(self.fov_y_deg * fx.fov_scale).to_radians(),
			aspect,
			self.near,
			self.far,
		);
		proj * view
	}
}
