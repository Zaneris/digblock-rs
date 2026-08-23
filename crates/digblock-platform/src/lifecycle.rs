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
pub(crate) enum Phase {
	Uninitialized,
	Running,
	Suspended,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
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
	/// Nobody can see the window: not transient like a timeout, so the retry has to be paced.
	SurfaceOccluded,
	/// Any other surface error.
	SurfaceFatal,
	/// The device-lost callback fired, possibly off-thread; the shell feeds it on the main thread.
	DeviceLost,
	/// A surface was (re)created with this format; a different one on resume orders a rebuild.
	SurfaceFormatPicked(wgpu::TextureFormat),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
	/// First launch: window + gpu + surface + `Game::init`.
	InitAll,
	/// Pause clock, deliver FocusLost + `on_suspend`, drop the surface.
	SuspendGame,
	/// Recreate the surface (and window if the OS killed it), notify resize + `on_resume`.
	RecreateSurface,
	/// `Game::rebuild_gpu_resources` on the current device.
	RebuildRenderer,
	/// Full recovery: new adapter/device/surface, then rebuild.
	ReinitDevice,
	/// Reconfigure the existing surface (lost/outdated).
	ReconfigureSurface,
	/// Transient (timeout): just request another frame.
	RetryNextFrame,
	/// Occluded: idle a beat, then ask again — asking as fast as the CPU allows burns a core.
	IdleThenRetry,
	Exit,
}

/// Ordered actions for one event (a short list today).
pub(crate) type Actions = Vec<Action>;

/// Frames a fatal acquire error is treated as loss-suspect (retried)
/// before giving up. A dying device surfaces as a generic fatal error
/// before wgpu's lost callback is pumped; each retry frame lets in-flight
/// work drain so the shell's next poll can fire the closure.
const MAX_FATAL_RETRIES: u8 = 2;

pub(crate) struct Lifecycle {
	phase: Phase,
	/// The format the game's pipelines were built against (init or last
	/// rebuild).
	built_format: Option<wgpu::TextureFormat>,
	/// Device loss reported while Suspended (no recovery possible):
	/// deferred to the next resume. This is exclusively the
	/// Suspended-deferral latch — while Running, loss recovers
	/// immediately and never sets it.
	device_lost: bool,
	/// Consecutive loss-suspect retries granted for fatal acquire errors
	/// since the last successful frame boundary (recovery, rebuild, or
	/// resume).
	fatal_retries: u8,
}

impl Default for Lifecycle {
	fn default() -> Self {
		Self::new()
	}
}

impl Lifecycle {
	pub(crate) fn new() -> Self {
		Self {
			phase: Phase::Uninitialized,
			built_format: None,
			device_lost: false,
			fatal_retries: 0,
		}
	}

	/// Test seam: the shell mirrors phase through its own AppState and
	/// never asks.
	#[cfg(test)]
	pub(crate) fn phase(&self) -> Phase {
		self.phase
	}

	/// Pure transition: updates the phase and returns the ordered actions
	/// the shell must execute. Idempotent where the OS is (double-resume,
	/// double-suspend deliver nothing).
	pub(crate) fn on(&mut self, ev: Event) -> Actions {
		let mut out = Actions::new();
		match (self.phase, ev) {
			(Phase::Uninitialized, Event::OsResumed) => {
				self.phase = Phase::Running;
				out.push(Action::InitAll);
			}
			(Phase::Suspended, Event::OsResumed | Event::ToggleDrillSuspend) => {
				self.phase = Phase::Running;
				self.fatal_retries = 0;
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
			(Phase::Running, Event::SurfaceOccluded) => out.push(Action::IdleThenRetry),
			(Phase::Running, Event::SurfaceFatal) => {
				// A dying device surfaces (sic) as a generic fatal acquire
				// error before wgpu's lost callback is pumped (the closure
				// only fires from device maintenance once the queue drains
				// — the shell polls, but in-flight work can delay it a
				// frame). Treat fatal as loss-suspect: retry a bounded
				// number of frames so a trailing DeviceLost can win; exit
				// if it never arrives.
				if self.fatal_retries < MAX_FATAL_RETRIES {
					self.fatal_retries += 1;
					out.push(Action::RetryNextFrame);
				} else {
					out.push(Action::Exit);
				}
			}
			(Phase::Running, Event::DeviceLost) => {
				self.device_lost = false;
				self.fatal_retries = 0;
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
	pub(crate) fn note_rebuilt(&mut self, format: wgpu::TextureFormat) {
		self.built_format = Some(format);
		self.fatal_retries = 0;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use wgpu::TextureFormat::{Bgra8Unorm, Rgba8Unorm};

	fn acts(l: &mut Lifecycle, ev: Event) -> Vec<Action> {
		l.on(ev).into_iter().collect()
	}

	/// Spend the whole fatal-retry allowance and assert it ends in an
	/// exit. Walked rather than spelled out, so the three tests that
	/// exhaust the budget move together when it is retuned.
	fn spend_the_fatal_budget(l: &mut Lifecycle) {
		for i in 0..MAX_FATAL_RETRIES {
			assert_eq!(
				acts(l, Event::SurfaceFatal),
				vec![Action::RetryNextFrame],
				"retry {i} of the allowance"
			);
		}
		assert_eq!(acts(l, Event::SurfaceFatal), vec![Action::Exit]);
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
		// Occlusion asks again too, but never at the top of a redraw
		// chain — a covered window would otherwise cost a whole core.
		assert_eq!(
			acts(&mut l, Event::SurfaceOccluded),
			vec![Action::IdleThenRetry]
		);
		// ...and unlike a fatal error it never runs out of patience: it
		// clears when somebody moves a window, which may be never.
		for _ in 0..MAX_FATAL_RETRIES + 3 {
			assert_eq!(
				acts(&mut l, Event::SurfaceOccluded),
				vec![Action::IdleThenRetry]
			);
		}
		// Fatal is loss-suspect: bounded retries, then exit. Giving up
		// is an Exit ACTION, not a phase change — the app is still
		// Running when it is handed one, and the caller decides.
		spend_the_fatal_budget(&mut l);
		assert_eq!(l.phase(), Phase::Running);
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
	fn surface_fatal_then_late_device_lost_recovers() {
		// The real destroy ordering: the acquire fails before the lost
		// callback is pumped, then DeviceLost lands a frame later.
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		assert_eq!(
			acts(&mut l, Event::SurfaceFatal),
			vec![Action::RetryNextFrame]
		);
		assert_eq!(acts(&mut l, Event::DeviceLost), vec![Action::ReinitDevice]);
		// The shell calls note_rebuilt after ReinitDevice completes; the
		// retry budget is restored for any future incident.
		l.note_rebuilt(Bgra8Unorm);
		assert_eq!(
			acts(&mut l, Event::SurfaceFatal),
			vec![Action::RetryNextFrame]
		);
		assert_eq!(l.phase(), Phase::Running);
	}

	#[test]
	fn fatal_retry_budget_resets_on_resume() {
		let mut l = Lifecycle::new();
		l.on(Event::OsResumed);
		assert_eq!(
			acts(&mut l, Event::SurfaceFatal),
			vec![Action::RetryNextFrame]
		);
		l.on(Event::OsSuspended);
		l.on(Event::OsResumed);
		// Fresh budget after the resume: the full allowance again, even
		// though one retry was already spent before the suspend.
		spend_the_fatal_budget(&mut l);
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
			SurfaceOccluded,
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
