//! Thin wasm launcher — the browser analogue of apps/desktop/src/main.rs.
//! All real work lives in digblock-render/digblock-platform; this file
//! parses URL query params into an `EngineConfig` and starts the loop.
#![cfg(target_arch = "wasm32")]

use digblock_render::{Engine, EngineConfig};
use wasm_bindgen::prelude::*;

mod audio;
mod jobs;
mod services;
mod settings;
mod store;

fn param<T: std::str::FromStr>(p: &web_sys::UrlSearchParams, key: &str) -> Option<T> {
	p.get(key).and_then(|v| v.parse().ok())
}

/// `?flag` and `?flag=1` both count; `?flag=0` doesn't.
fn flag(p: &web_sys::UrlSearchParams, key: &str) -> bool {
	p.get(key).is_some_and(|v| v != "0")
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
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
	// Saved settings first, explicit query params on top (URL > saved >
	// default — the desktop CLI precedence).
	let saved = settings::load(digblock_render::settings::Settings {
		draw_dist: config.draw_dist,
		music_vol: config.music_vol,
		sfx_vol: config.sfx_vol,
		ssr: config.ssr,
		ui_scale: config.ui_scale,
		render_scale: config.render_scale,
		gi_scale_div: config.gi_scale_div,
		name: String::new(),
	});
	config.draw_dist = saved.draw_dist;
	config.music_vol = saved.music_vol;
	config.sfx_vol = saved.sfx_vol;
	config.ssr = saved.ssr;
	config.ui_scale = saved.ui_scale;
	// Density picks both only when NEITHER was saved — the engine applies
	// them at the first `set_scale_factor`, where the backing scale is
	// first known (see `gfx_defaults_auto`).
	config.gfx_defaults_auto =
		saved.render_scale == config.render_scale && saved.gi_scale_div == config.gi_scale_div;
	config.render_scale = saved.render_scale;
	config.gi_scale_div = saved.gi_scale_div;
	if let Some(r) = param::<f32>(&params, "renderscale") {
		config.render_scale = r.clamp(
			digblock_render::settings::RENDER_SCALE_MIN,
			digblock_render::settings::RENDER_SCALE_MAX,
		);
	}
	if let Some(d) = param::<u32>(&params, "giscale") {
		if digblock_render::settings::GI_SCALE_DIVISORS.contains(&d) {
			config.gi_scale_div = d;
			config.gfx_defaults_auto = false;
		}
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
	// The capture harness, same fields the desktop CLI drives. This is the
	// only way to compare the two platforms honestly: the scripted pose is
	// a pure function of the frame index, so `?warp=600` lands the browser
	// on bit-identically the camera `--warp 600` lands the desktop on.
	//
	// ?warp is the one that measures RENDER cost: ?bench travels at 30
	// blocks/s, which is a STREAMING regime, so a travelling flight is
	// dominated by how fast chunks arrive and says little about the GI.
	// (That used to be near-total — jobs ran on the main thread here —
	// and is merely large now that a worker pool runs them.) Pair ?bench
	// with ?frames=N: the summary is emitted at that boundary.
	config.bench = flag(&params, "bench");
	// A capture path must not sit on a menu screen. Desktop gets this for
	// free — `EngineConfig::desktop()` starts menu-less — but the web
	// default is a menu like an interactive launch, so `?bench` opened the
	// title screen and the scripted flight never ran: a summary line with
	// `fill n/a` over a backdrop world, measuring nothing.
	if config.bench {
		config.menu = false;
	}
	if let Some(n) = param::<u32>(&params, "fly") {
		config.fly = Some(n);
	}
	if let Some(n) = param::<u32>(&params, "warp") {
		config.fly = Some(n);
		config.warp = true;
	}
	config.walk = flag(&params, "walk");
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
	// ?name= is an explicit override, still held to the 3-10 char rule —
	// an invalid one is treated as absent; otherwise the remembered
	// last-used name pre-fills the menu field.
	config.player_name = params
		.get("name")
		.filter(|n| digblock_net::protocol::validate_name(n).is_ok())
		.or_else(|| (!saved.name.is_empty()).then(|| saved.name.clone()));

	if flag(&params, "join") {
		config.auto_join = Some(params.get("server").unwrap_or_default());
		// The menu refuses to join without a valid name.
		if config.player_name.is_none() {
			config.player_name = Some("web".into());
		}
	}

	config.identity = load_identity();

	// The production master is the default; ?master= overrides for dev
	// stacks, and an explicitly empty ?master= disables multiplayer + music
	// entirely (no master, no server list, no token mint).
	let master_url = match params.get("master") {
		Some(m) if m.is_empty() => None,
		Some(m) => Some(m),
		None => Some(digblock_net::protocol::DEFAULT_MASTER_URL.to_string()),
	};
	let workers = config.workers;
	let mut engine = Engine::new(config);
	engine.set_settings_store(Box::new(settings::save));
	engine.set_world_store(Box::new(store::LocalStorageWorldStore::new()));
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
	// first click/keydown (Chrome's autoplay gesture rule).
	if !flag(&params, "mute") {
		engine.set_audio(audio::start(master_url.clone()));
	}
	if let Some(master_url) = master_url {
		engine.set_menu_services(services::start(master_url));
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

/// Load-or-create the persistent identity secret (32 hex chars in
/// localStorage). Private-mode/blocked storage degrades to a random
/// per-session identity — the game still runs, persistence doesn't.
fn load_identity() -> [u8; 16] {
	let fresh = |storage: Option<&web_sys::Storage>| -> [u8; 16] {
		let mut id = [0u8; 16];
		let filled = web_sys::window()
			.and_then(|w| w.crypto().ok())
			.and_then(|c| c.get_random_values_with_u8_array(&mut id).ok())
			.is_some();
		if !filled {
			// No crypto object (ancient browser): weak fallback, still
			// unique enough for a per-session identity.
			for (i, b) in id.iter_mut().enumerate() {
				*b = (js_sys::Math::random() * 256.0) as u8 ^ (i as u8);
			}
		}
		if let Some(s) = storage {
			let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
			let _ = s.set_item("digblock.identity", &hex);
		}
		id
	};
	let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else {
		return fresh(None);
	};
	match storage.get_item("digblock.identity") {
		Ok(Some(hex)) if hex.len() == 32 => {
			let mut id = [0u8; 16];
			let ok = (0..16).all(|i| {
				u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
					.map(|b| id[i] = b)
					.is_ok()
			});
			if ok && id != [0; 16] {
				id
			} else {
				fresh(Some(&storage))
			}
		}
		_ => fresh(Some(&storage)),
	}
}
