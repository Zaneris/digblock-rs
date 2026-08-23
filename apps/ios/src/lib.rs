use digblock_client::{Engine, EngineConfig};
use digblock_platform::RunOptions;

/// Called from main.m. winit's run_app invokes UIApplicationMain internally and never returns.
/// `unsafe(no_mangle)` is the obligation spelled out: an unmangled symbol promises that nothing
/// else in the final binary claims this name, and the compiler cannot check it. `main.m` is the
/// only caller.
#[unsafe(no_mangle)]
pub extern "C" fn digblock_main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
	let options = RunOptions {
		title: "DigBlock".into(),
		..Default::default()
	};
	// run never returns on iOS (UIApplicationMain); the drill result is
	// desktop-only.
	let _ = digblock_platform::run(Engine::new(EngineConfig::mobile()), options);
}
