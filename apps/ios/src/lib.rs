use digblock_platform::RunOptions;
use digblock_render::{Engine, EngineConfig};

/// Called from main.m. winit's run_app invokes UIApplicationMain internally
/// and never returns.
#[no_mangle]
pub extern "C" fn digblock_main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
	let options = RunOptions {
		title: "DigBlock".into(),
		..Default::default()
	};
	digblock_platform::run(Engine::new(EngineConfig::mobile()), options);
}
