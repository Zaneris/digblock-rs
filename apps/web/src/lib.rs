//! Thin wasm launcher — the browser analogue of apps/desktop/src/main.rs.
//! All real work lives in digblock-render/digblock-platform; this file
//! parses URL query params into an `EngineConfig` and starts the loop.
#![cfg(target_arch = "wasm32")]

use digblock_render::{Engine, EngineConfig};
use wasm_bindgen::prelude::*;

mod audio;
mod auth;
mod jobs;
mod services;
mod settings;
mod store;

/// A query param that carries a value, absent if the key isn't there.
///
/// A key that IS there but doesn't parse is fatal, never a silent fall
/// back to the default. These select bench arms — `?workers=`, `?frames=`,
/// `?warp=` — and a typo that quietly ran the shipped default gets
/// recorded in BENCH.md as the arm that was asked for. Same rule the
/// engine's other capture knobs follow (a bad `DIG_STUB` term is fatal;
/// desktop exits 2 on a bad flag value); on wasm the panic hook prints it
/// to the console, which is this platform's version of exiting loudly.
fn param<T: std::str::FromStr>(p: &web_sys::UrlSearchParams, key: &str) -> Option<T> {
	let raw = p.get(key)?;
	match raw.parse() {
		Ok(v) => Some(v),
		Err(_) => panic!(
			"?{key}={raw:?} is not a valid {}",
			std::any::type_name::<T>()
		),
	}
}

/// `?flag` and `?flag=1` both count; `?flag=0` doesn't.
fn flag(p: &web_sys::UrlSearchParams, key: &str) -> bool {
	p.get(key).is_some_and(|v| v != "0")
}

/// Boot is async now: a sign-in that came back has to be redeemed with the
/// master BEFORE the engine is built, or the menu would open signed out and
/// then jump. `digblock_platform::run` returns immediately on wasm (winit
/// keeps the loop alive through the event system), so nothing is lost by
/// starting it from a task.
#[wasm_bindgen(start)]
pub fn start() {
	wasm_bindgen_futures::spawn_local(async {
		if let Err(e) = boot().await {
			log::error!("startup failed: {e:?}");
		}
	});
}

async fn boot() -> Result<(), JsValue> {
	console_error_panic_hook::set_once();
	let _ = console_log::init_with_level(log::Level::Info);

	let params = web_sys::window()
		.and_then(|w| w.location().search().ok())
		.map(|s| web_sys::UrlSearchParams::new_with_str(&s).expect("parse query"))
		.expect("window.location");

	// Query params mirror the desktop flags: ?seed=, ?tod=, ?draw=,
	// ?ssr=0, ?overlay, ?name=, ?master= (menu multiplayer),
	// ?join / ?server= (non-interactive join for scripted flows). The
	// defaults are full desktop quality — these exist to opt weaker
	// hardware down.
	let mut config = EngineConfig::web();
	config.seed = param(&params, "seed");
	// Dev builds only (like desktop --tod): in multiplayer the server
	// dictates the world clock. `just web` builds release, so the served
	// bundle ignores ?tod=.
	if cfg!(debug_assertions) {
		config.tod = param(&params, "tod");
	}
	// The capture harness, same fields the desktop CLI drives. This is the
	// only way to compare the two platforms honestly: the scripted pose is
	// a pure function of the frame index, so `?warp=600` lands the browser
	// on bit-identically the camera `--warp 600` lands the desktop on.
	//
	// ?warp is the one that measures RENDER cost: ?bench travels at the
	// flyover's default speed, which is a STREAMING regime, so a travelling
	// flight is dominated by how fast chunks arrive and says little about
	// the GI.
	// (That used to be near-total — jobs ran on the main thread here —
	// and is merely large now that a worker pool runs them.) Pair ?bench
	// with ?frames=N: the summary is emitted at that boundary.
	config.bench = flag(&params, "bench");
	if let Some(n) = param::<u32>(&params, "fly") {
		config.fly = Some(n);
	}
	if let Some(n) = param::<u32>(&params, "warp") {
		config.fly = Some(n);
		config.warp = true;
	}
	config.walk = flag(&params, "walk");

	// ONE predicate, exactly as desktop derives it (`apps/desktop/main.rs`),
	// rather than a guard per capture flag. A capture path gets no menu, no
	// audio, no master traffic, and neither loads nor saves settings — the
	// per-flag version of this shipped covering `?bench` alone, which left
	// `?warp`/`?fly` running their scripted flight behind the title
	// screen's dim, and left every capture reading (and WRITING) whatever
	// draw distance and lighting detail this browser happened to have
	// saved. Two runs of the same arm have to measure the same thing.
	let interactive = !config.bench && config.fly.is_none();
	config.menu = interactive;
	// Also what the engine reads to decide whether to listen to input
	// and whether to ask for pointer lock. On the web the lock request
	// needs a user gesture and Chrome simply refuses one from rAF, so a
	// capture never held the pointer here the way desktop did — but a
	// scripted `?fly` run still accepted keystrokes, which is the same
	// bug with a quieter symptom.
	config.interactive = interactive;

	// Saved settings apply to interactive runs only; explicit query params
	// win over both (URL > saved > default — the desktop CLI precedence).
	// A capture keeps the shipped defaults, which is what makes two runs of
	// the same arm comparable on a browser that has ever been played on.
	let defaults = digblock_render::settings::Settings {
		draw_dist: config.draw_dist,
		music_vol: config.music_vol,
		sfx_vol: config.sfx_vol,
		ssr: config.ssr,
		ui_scale: config.ui_scale,
		render_scale: config.render_scale,
		gi_scale_div: config.gi_scale_div,
	};
	let saved = if interactive {
		settings::load(defaults)
	} else {
		defaults
	};
	config.draw_dist = saved.draw_dist;
	config.music_vol = saved.music_vol;
	config.sfx_vol = saved.sfx_vol;
	config.ssr = saved.ssr;
	config.ui_scale = saved.ui_scale;
	// Density picks both only when NEITHER was saved — the engine applies
	// them at the first `set_scale_factor`, where the backing scale is
	// first known (see `gfx_defaults_auto`). Never on a capture path: those
	// keep the shipped values so no baseline moves with the display.
	config.gfx_defaults_auto = interactive
		&& saved.render_scale == config.render_scale
		&& saved.gi_scale_div == config.gi_scale_div;
	config.render_scale = saved.render_scale;
	config.gi_scale_div = saved.gi_scale_div;
	if let Some(r) = param::<f32>(&params, "renderscale") {
		config.render_scale = r.clamp(
			digblock_render::settings::RENDER_SCALE_MIN,
			digblock_render::settings::RENDER_SCALE_MAX,
		);
	}
	if let Some(d) = param::<u32>(&params, "giscale")
		&& digblock_render::settings::GI_SCALE_DIVISORS.contains(&d)
	{
		config.gi_scale_div = d;
		config.gfx_defaults_auto = false;
	}
	if let Some(d) = param::<i32>(&params, "draw") {
		config.draw_dist = d.clamp(10, 32);
	}
	if params.get("ssr").is_some() {
		config.ssr = flag(&params, "ssr");
	}
	if let Some(u) = param::<f32>(&params, "uiscale") {
		config.ui_scale = u.clamp(
			digblock_render::settings::UI_SCALE_MIN,
			digblock_render::settings::UI_SCALE_MAX,
		);
	}
	config.overlay = flag(&params, "overlay");
	// Chunk jobs run in Web Workers when the machine has cores to spare
	// (jobs.rs); `?workers=N` overrides for A/B, and 0 is the pre-pool
	// inline path exactly. The pipeline is told nothing — it upgrades when
	// a pool is attached below — so this only records what to start, and
	// what the debug overlay reports.
	config.workers = param(&params, "workers").map_or_else(jobs::default_count, |n: usize| {
		n.min(jobs::WORKERS_HARD_MAX)
	});
	// The web twin of desktop's DIG_SPIKELOG: wasm has no environment, and
	// this is the platform whose main-thread spikes most need attributing.
	config.spike_log = flag(&params, "spikelog");
	let join = flag(&params, "join");
	if join {
		config.auto_join = Some(params.get("server").unwrap_or_default());
	}

	// The production master is the default; ?master= overrides for dev
	// stacks, and an explicitly empty ?master= disables multiplayer + music
	// entirely (no master, no server list, no token mint). The default
	// applies to interactive runs and joins only — a capture path must not
	// ping the master.
	let master_url = match params.get("master") {
		Some(m) if m.is_empty() => None,
		Some(m) => Some(m),
		None if interactive || join => Some(digblock_net::protocol::DEFAULT_MASTER_URL.to_string()),
		None => None,
	};
	// A sign-in that came back lands here as ?code=&state=. Redeem it
	// before the engine exists, so the menu opens already signed in and
	// the code leaves the address bar immediately.
	let session = match (&master_url, auth::take_callback()) {
		(Some(master_url), Some((code, verifier))) => {
			match auth::exchange(master_url, &code, &verifier).await {
				Ok(s) => {
					auth::store(&s);
					Some(s)
				}
				Err(e) => {
					log::warn!("sign-in could not be completed: {e}");
					auth::load()
				}
			}
		}
		_ => auth::load(),
	};
	// A scripted join cannot go through a browser round trip; ?devsignin=
	// gets it a session the way desktop's --dev-signin does.
	let session = match (&master_url, params.get("devsignin")) {
		(Some(master_url), Some(subject)) if !subject.is_empty() => {
			match auth::dev_sign_in(master_url, &subject).await {
				Ok(s) => {
					auth::store(&s);
					Some(s)
				}
				Err(e) => {
					log::error!("?devsignin failed: {e}");
					session
				}
			}
		}
		_ => session,
	};
	config.player_name = session.as_ref().and_then(|s| s.display_name.clone());
	config.account = session
		.as_ref()
		.map(|s| digblock_render::menu::AccountView {
			id: s.account_id.clone(),
			name: s.display_name.clone(),
		});

	let workers = config.workers;
	let mut engine = Engine::new(config);
	if interactive {
		// A capture run must not WRITE settings either: a `?renderscale=`
		// or `?draw=` on the URL would otherwise persist into every later
		// interactive session on this browser.
		engine.set_settings_store(Box::new(settings::save));
	}
	// Named local saves ride the menu; scripted joins and capture paths
	// stay store-less (ephemeral world, hermetic run).
	if interactive && !join {
		engine.set_world_store(Box::new(store::LocalStorageWorldStore::new()));
	}
	// Chunk jobs move to Web Workers when there are cores for them. The
	// pool is attached before the first frame but reports unready until a
	// worker has instantiated, so the opening seconds still stream inline
	// and nothing waits on a handshake.
	if workers > 0 {
		match worker_module_url() {
			Some(url) => {
				if let Some(pool) = jobs::WorkerPool::start(&url, workers) {
					engine.set_job_dispatch(Box::new(pool));
				}
			}
			// An older index.html, or the bundle served by hand: no pool,
			// and the client behaves exactly as it did before there was one.
			None => log::warn!("no worker module on the page; chunk jobs stay inline"),
		}
	}
	// ?mute kills the whole audio stack; otherwise sound starts on the
	// first click/keydown (Chrome's autoplay gesture rule). Audio rides
	// interactive runs only — the engine's sink stays None on a capture,
	// the same as desktop's.
	if interactive && !flag(&params, "mute") {
		engine.set_audio(audio::start(master_url.clone()));
	}
	engine.set_clipboard(Box::new(copy_to_clipboard));
	if let Some(master_url) = master_url {
		engine.set_menu_services(services::start(master_url, session));
	}
	digblock_platform::run(
		engine,
		digblock_platform::RunOptions {
			title: "DigBlock".into(),
			// `?frames=N` ends the run at frame N, which is what makes the
			// bench summary reachable here (see `?bench`): winit's exit on
			// wasm doesn't reliably run destructors, so the platform logs
			// the summary at that boundary rather than from `Drop`.
			exit_after_frames: param(&params, "frames"),
			..Default::default()
		},
	)
	.map_err(|e| JsValue::from_str(&e.to_string()))
}

/// The stamped worker module's URL, published on the global object by
/// index.html — the only file that knows a build stamp. Keeping it there
/// is what lets both modules be immutable-by-name: a stale index can ask
/// for a pair that no longer exists (a clean 404, self-healing on reload)
/// but never for half of a live one.
fn worker_module_url() -> Option<String> {
	js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("__digblockWorkerUrl"))
		.ok()
		.and_then(|v| v.as_string())
		.filter(|s| !s.is_empty())
}

/// The Account screen's Copy ID button.
///
/// `navigator.clipboard.writeText` only works inside the task that handled
/// the user's click, which is why the engine drains this from `input`
/// rather than `update`. The returned promise is deliberately dropped:
/// awaiting it would push the result into a later task, and there is
/// nothing useful to do with it.
fn copy_to_clipboard(text: &str) {
	let Some(window) = web_sys::window() else {
		return;
	};
	let _ = window.navigator().clipboard().write_text(text);
}
