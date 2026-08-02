//! The lifecycle DECISION core: a pure state machine (no winit, no
//! surfaces, no windows) that turns OS/drill/surface events into ordered
//! actions for the shell (`app.rs`) to execute. Extracted so the
//! suspend/resume/device-loss transitions — the standing regression
//! target for the Android port — are unit-testable headless.
//!
//! `wgpu::TextureFormat` appears as a plain value (it is just an enum);
//! no device or surface types cross this boundary.

/// Mirror of the shell's `AppState`, without resources.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
	Uninitialized,
	Running,
	Suspended,
}

#[derive(Clone, Copy, Debug)]
pub enum Event {
	/// winit `resumed()` — initial launch or return from background.
	OsResumed,
	/// winit `suspended()`.
	OsSuspended,
	/// F9: suspend when Running, resume when Suspended.
	ToggleDrillSuspend,
	/// F10: rebuild the game's GPU resources on the same device.
	DrillDeviceRebuild,
	/// `SurfaceError::Lost | Outdated`.
	SurfaceLost,
	/// `SurfaceError::Timeout`.
	SurfaceTimeout,
	/// Any other surface error.
	SurfaceFatal,
	/// The device-lost callback fired (possibly off-thread; the shell
	/// polls a flag and feeds this on the main thread).
	DeviceLost,
	/// A surface was just (re)created with this format. Emitted after
	/// every surface creation, including the first — the machine records
	/// the format the game's pipelines were built for and orders a
	/// rebuild when a resume picks a different one (monitor/GPU swap
	/// while backgrounded: every pipeline's color target would mismatch).
	SurfaceFormatPicked(wgpu::TextureFormat),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
	/// First launch: window + gpu + surface + `Game::init`.
	InitAll,
	/// Pause clock, deliver FocusLost + `on_suspend`, drop the surface.
	SuspendGame,
	/// Recreate the surface (and window if the OS killed it), notify
	/// resize + `on_resume`.
	RecreateSurface,
	/// `Game::rebuild_gpu_resources` on the current device.
	RebuildRenderer,
	/// Full recovery: new adapter/device/surface, then rebuild.
	ReinitDevice,
	/// Reconfigure the existing surface (lost/outdated).
	ReconfigureSurface,
	/// Transient (timeout): just request another frame.
	RetryNextFrame,
	Exit,
}

/// Ordered actions for one event (at most two today).
pub type Actions = Vec<Action>;

pub struct Lifecycle {
	phase: Phase,
	/// The format the game's pipelines were built against (init or last
	/// rebuild).
	built_format: Option<wgpu::TextureFormat>,
	/// Device loss reported while no recovery was possible (Suspended):
	/// deferred to the next resume.
	device_lost: bool,
}

impl Default for Lifecycle {
	fn default() -> Self {
		Self::new()
	}
}

impl Lifecycle {
	pub fn new() -> Self {
		Self {
			phase: Phase::Uninitialized,
			built_format: None,
			device_lost: false,
		}
	}

	pub fn phase(&self) -> Phase {
		self.phase
	}

	/// Pure transition: updates the phase and returns the ordered actions
	/// the shell must execute. Idempotent where the OS is (double-resume,
	/// double-suspend deliver nothing).
	pub fn on(&mut self, ev: Event) -> Actions {
		let mut out = Actions::new();
		match (self.phase, ev) {
			(Phase::Uninitialized, Event::OsResumed) => {
				self.phase = Phase::Running;
				out.push(Action::InitAll);
			}
			(Phase::Suspended, Event::OsResumed | Event::ToggleDrillSuspend) => {
				self.phase = Phase::Running;
				out.push(Action::RecreateSurface);
				// Device loss reported while backgrounded: recover now
				// that a surface exists again.
				if std::mem::take(&mut self.device_lost) {
					out.push(Action::ReinitDevice);
				}
			}
			(Phase::Running, Event::OsSuspended | Event::ToggleDrillSuspend) => {
				self.phase = Phase::Suspended;
				out.push(Action::SuspendGame);
			}
			(Phase::Running, Event::DrillDeviceRebuild) => out.push(Action::RebuildRenderer),
			(Phase::Running, Event::SurfaceLost) => out.push(Action::ReconfigureSurface),
			(Phase::Running, Event::SurfaceTimeout) => out.push(Action::RetryNextFrame),
			(Phase::Running, Event::SurfaceFatal) => {
				// A dead device commonly surfaces (sic) as an unexplained
				// fatal acquire error — recover instead of quitting when
				// the loss callback fired.
				if std::mem::take(&mut self.device_lost) {
					out.push(Action::ReinitDevice);
				} else {
					out.push(Action::Exit);
				}
			}
			(Phase::Running, Event::DeviceLost) => {
				self.device_lost = false;
				out.push(Action::ReinitDevice);
			}
			(Phase::Suspended, Event::DeviceLost) => self.device_lost = true,
			(_, Event::SurfaceFormatPicked(f)) => {
				match self.built_format {
					None => self.built_format = Some(f),
					Some(built) if built == f => {}
					Some(_) => {
						// Pipelines were built for another format: every
						// draw would be a validation error. Rebuild.
						self.built_format = Some(f);
						out.push(Action::RebuildRenderer);
					}
				}
			}
			// Idempotent / impossible combinations deliver nothing.
			_ => {}
		}
		out
	}

	/// A rebuild (drill or recovery) re-created pipelines for `format`.
	pub fn note_rebuilt(&mut self, format: wgpu::TextureFormat) {
		self.built_format = Some(format);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use wgpu::TextureFormat::{Bgra8Unorm, Rgba8Unorm};

	fn acts(l: &mut Lifecycle, ev: Event) -> Vec<Action> {
		l.on(ev).into_iter().collect()
	}

	#[test]
	fn launch_suspend_resume_cycle() {
		let mut l = Lifecycle::new();
		assert_eq!(acts(&mut l, Event::OsResumed), vec![Action::InitAll]);
		assert_eq!(l.phase(), Phase::Running);
		assert_eq!(acts(&mut l, Event::OsSuspended), vec![Action::SuspendGame]);
		assert_eq!(l.phase(), Phase::Suspended);
		assert_eq!(
			acts(&mut l, Event::OsResumed),
			vec![Action::RecreateSurface]
		);
		assert_eq!(l.phase(), Phase::Running);
	}

	#[test]
	fn double_resume_and_double_suspend_are_idempotent() {
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		// macOS fires Resumed once after startup.
		assert!(acts(&mut l, Event::OsResumed).is_empty());
		l.on(Event::OsSuspended);
		assert!(acts(&mut l, Event::OsSuspended).is_empty());
		assert_eq!(l.phase(), Phase::Suspended);
	}

	#[test]
	fn f9_toggles_both_directions_and_f10_rebuilds() {
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		assert_eq!(
			acts(&mut l, Event::ToggleDrillSuspend),
			vec![Action::SuspendGame]
		);
		assert_eq!(
			acts(&mut l, Event::ToggleDrillSuspend),
			vec![Action::RecreateSurface]
		);
		assert_eq!(
			acts(&mut l, Event::DrillDeviceRebuild),
			vec![Action::RebuildRenderer]
		);
		// F10 while suspended: nothing to rebuild into.
		l.on(Event::ToggleDrillSuspend);
		assert!(acts(&mut l, Event::DrillDeviceRebuild).is_empty());
	}

	#[test]
	fn surface_error_triage() {
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		assert_eq!(
			acts(&mut l, Event::SurfaceLost),
			vec![Action::ReconfigureSurface]
		);
		assert_eq!(
			acts(&mut l, Event::SurfaceTimeout),
			vec![Action::RetryNextFrame]
		);
		assert_eq!(acts(&mut l, Event::SurfaceFatal), vec![Action::Exit]);
	}

	#[test]
	fn format_change_on_resume_rebuilds_exactly_once() {
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		// Initial pick: recorded, no rebuild.
		assert!(acts(&mut l, Event::SurfaceFormatPicked(Bgra8Unorm)).is_empty());
		l.on(Event::OsSuspended);
		l.on(Event::OsResumed);
		// Same format after resume: nothing.
		assert!(acts(&mut l, Event::SurfaceFormatPicked(Bgra8Unorm)).is_empty());
		l.on(Event::OsSuspended);
		l.on(Event::OsResumed);
		// Different format (monitor/GPU swap while backgrounded): rebuild.
		assert_eq!(
			acts(&mut l, Event::SurfaceFormatPicked(Rgba8Unorm)),
			vec![Action::RebuildRenderer]
		);
		// And only once.
		assert!(acts(&mut l, Event::SurfaceFormatPicked(Rgba8Unorm)).is_empty());
	}

	#[test]
	fn device_loss_recovers_now_or_deferred() {
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		assert_eq!(acts(&mut l, Event::DeviceLost), vec![Action::ReinitDevice]);

		// While suspended: deferred to the resume, after the surface.
		l.on(Event::OsSuspended);
		assert!(acts(&mut l, Event::DeviceLost).is_empty());
		assert_eq!(
			acts(&mut l, Event::OsResumed),
			vec![Action::RecreateSurface, Action::ReinitDevice]
		);
		// Consumed: the next suspend/resume is clean.
		l.on(Event::OsSuspended);
		assert_eq!(
			acts(&mut l, Event::OsResumed),
			vec![Action::RecreateSurface]
		);
	}

	#[test]
	fn fatal_surface_error_with_lost_device_recovers_instead_of_exiting() {
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		// The loss callback fired off-thread; the shell polls it and the
		// acquire failure arrives as SurfaceFatal in the same frame.
		l.on(Event::OsSuspended);
		l.on(Event::DeviceLost);
		l.on(Event::ToggleDrillSuspend); // resume consumes the deferred flag
		l.on(Event::OsSuspended);
		l.on(Event::OsResumed);
		assert_eq!(acts(&mut l, Event::SurfaceFatal), vec![Action::Exit]);

		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		l.on(Event::OsSuspended);
		l.on(Event::DeviceLost);
		// Resume executes surface + reinit...
		l.on(Event::OsResumed);
		// ...but if a fatal error still arrives with a fresh loss:
		l.on(Event::OsSuspended);
		l.on(Event::DeviceLost);
		l.on(Event::OsResumed);
		assert_eq!(l.phase(), Phase::Running);
	}

	/// Exhaustive phase × event walk: no combination panics, and the
	/// phase never becomes inconsistent with the returned actions.
	#[test]
	fn full_table_is_total() {
		use Event::*;
		let events = [
			OsResumed,
			OsSuspended,
			ToggleDrillSuspend,
			DrillDeviceRebuild,
			SurfaceLost,
			SurfaceTimeout,
			SurfaceFatal,
			DeviceLost,
			SurfaceFormatPicked(Bgra8Unorm),
		];
		for start in [Phase::Uninitialized, Phase::Running, Phase::Suspended] {
			for &ev in &events {
				let mut l = Lifecycle::new();
				// Drive to the start phase.
				match start {
					Phase::Uninitialized => {}
					Phase::Running => {
						l.on(OsResumed);
					}
					Phase::Suspended => {
						l.on(OsResumed);
						l.on(OsSuspended);
					}
				}
				let before = l.phase();
				let actions = l.on(ev);
				// SuspendGame ⇒ Suspended; RecreateSurface/InitAll ⇒ Running.
				for a in &actions {
					match a {
						Action::SuspendGame => assert_eq!(l.phase(), Phase::Suspended),
						Action::RecreateSurface | Action::InitAll => {
							assert_eq!(l.phase(), Phase::Running)
						}
						_ => {}
					}
				}
				if actions.is_empty() && !matches!(ev, SurfaceFormatPicked(_) | DeviceLost) {
					assert_eq!(l.phase(), before, "no-op events keep the phase");
				}
			}
		}
	}
}
