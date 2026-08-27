//! Thin wasm launcher — the browser analogue of apps/desktop/src/main.rs.
//! All real work lives in digblock-client/digblock-platform; this file
//! parses URL query params into an `EngineConfig` and starts the loop.
#![cfg(target_arch = "wasm32")]

use digblock_client::{Engine, EngineConfig};
use wasm_bindgen::prelude::*;

mod audio;
mod auth;
mod jobs;
mod services;
mod settings;
mod store;

/// A query param that carries a value, absent if the key isn't there. A key that IS there but
/// doesn't parse is fatal, never a silent fall back to the default: these select bench arms, and a
/// typo that quietly ran the shipped default gets recorded in BENCH.md as the arm that was asked
/// for — the rule every other capture knob follows. On wasm the panic hook prints to the console,
/// which is this platform's version of exiting loudly.
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

	// This task is queued AHEAD of the page's own `await init()`
	// continuation, and nothing below yields before `Engine::new` — so
	// without handing the browser one frame here, the loading overlay's
	// "Starting…" stage could never paint before the engine was built.
	digblock_platform::yield_to_browser().await;

	// A sign-in that came back lands here as `?code=&state=` and NOTHING else — the master
	// replaces the redirect target's query with its own. `take_callback` puts back the query that
	// configured this page, so it has to run BEFORE the params are read, or `?master=` is lost and
	// the code is redeemed against the production default instead of the master that minted it.
	let callback = auth::take_callback();

	let params = web_sys::window()
		.and_then(|w| w.location().search().ok())
		.map(|s| web_sys::UrlSearchParams::new_with_str(&s).expect("parse query"))
		.expect("window.location");
	reject_unknown_params(&params);

	// Query params mirror the desktop flags; the defaults are full desktop quality, and these
	// exist to opt weaker hardware down. Except on a phone or tablet, where the base config is
	// the mobile one: `?mobile` / `?mobile=0` overrides the detection both ways (the URL wins, as
	// everywhere), which is also how desktop A/Bs the mobile defaults without a device in hand.
	let mobile = params
		.get("mobile")
		.map_or_else(is_mobile_browser, |v| v != "0");
	let mut config = if mobile {
		EngineConfig::web_mobile()
	} else {
		EngineConfig::web()
	};
	config.seed = param(&params, "seed");
	// The pose knobs, spelled as the desktop flags are: `?pos=x,y,z`,
	// `?yaw=`, `?pitch=`. Read BEFORE the scene knobs, whose siphon arm
	// defaults the pitch only when none was asked for.
	if let Some(v) = params.get("pos") {
		let mut it = v.split(',').map(|c| c.trim().parse::<f32>());
		match (it.next(), it.next(), it.next(), it.next()) {
			(Some(Ok(x)), Some(Ok(y)), Some(Ok(z)), None) => config.start_pos = Some([x, y, z]),
			_ => panic!("?pos={v:?} wants x,y,z"),
		}
	}
	config.yaw = param(&params, "yaw");
	config.pitch = param(&params, "pitch");
	// `?frames=N` is this platform's `--exit`: the one way a run ends on
	// its own, and what the harness waits for.
	let frames: Option<u64> = param(&params, "frames");
	// Dev builds, or a run that ends itself — the desktop `--tod` rule. In
	// multiplayer the server dictates the world clock, and `just web`
	// builds release, so an open-ended page cannot honour ?tod=. It
	// REFUSES rather than dropping it, as the desktop does: a frozen sun
	// silently ignored photographs the wrong hour and says nothing.
	let tod: Option<f32> = param(&params, "tod");
	if tod.is_some() {
		assert!(
			cfg!(debug_assertions) || frames.is_some(),
			"?tod needs a dev build, or ?frames (a capture run that quits on its own)"
		);
		config.tod = tod;
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

	// ONE predicate, exactly as desktop derives it (`apps/desktop/main.rs`), rather than a guard
	// per capture flag: a capture path gets no menu, no audio, no master traffic, and neither
	// loads nor saves settings, so two runs of the same arm measure the same thing. `?capture` is
	// the web's `--screenshot` — the hermetic, menu-less, input-deaf run — while `?play` keeps
	// everything interactive and merely skips the menu, for a harness that presses a key.
	let capture = flag(&params, "capture");
	let interactive = !config.bench && config.fly.is_none() && !capture;
	config.menu = interactive && !flag(&params, "play");
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
	let defaults = digblock_client::settings::Settings {
		draw_dist: config.draw_dist,
		music_vol: config.music_vol,
		sfx_vol: config.sfx_vol,
		ssr: config.ssr,
		ui_scale: config.ui_scale,
		render_scale: config.render_scale,
		gi_scale_div: config.gi_scale_div,
		max_lights: config.max_lights,
		touch_jump_button: config.touch_jump_button,
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
	config.touch_jump_button = saved.touch_jump_button;
	// The engine picks these only when NONE was saved, at the first
	// `set_scale_factor`, where the backing scale is first known (see
	// `gfx_defaults_auto`). Never on a capture path: those keep the shipped
	// values so no baseline moves with the display. Never on the mobile
	// base either: it already IS the density decision — and its own shorter
	// light list, which the desktop default must not raise — so a second
	// authority re-deriving it could only agree or fight.
	config.gfx_defaults_auto = interactive
		&& !mobile
		&& saved.render_scale == config.render_scale
		&& saved.gi_scale_div == config.gi_scale_div
		&& saved.max_lights == config.max_lights;
	// After the test, like its two siblings: each is compared against the
	// base it would overwrite.
	config.render_scale = saved.render_scale;
	config.gi_scale_div = saved.gi_scale_div;
	config.max_lights = saved.max_lights;
	if let Some(r) = param::<f32>(&params, "renderscale") {
		config.render_scale = r.clamp(
			digblock_client::settings::RENDER_SCALE_MIN,
			digblock_client::settings::RENDER_SCALE_MAX,
		);
	}
	if let Some(d) = param::<u32>(&params, "giscale")
		&& digblock_client::settings::lever_gi_div_ok(d)
	{
		config.gi_scale_div = d;
		config.gfx_defaults_auto = false;
	}
	if let Some(d) = param::<i32>(&params, "draw") {
		// The FLOOR, not the default UI minimum: the URL is the dev lever,
		// and this is how a desktop browser A/Bs the mobile profile's
		// lower rungs without a device in hand.
		config.draw_dist = d.clamp(
			digblock_client::settings::DRAW_FLOOR,
			digblock_client::settings::DRAW_MAX,
		);
	}
	if let Some(n) = param::<i32>(&params, "maxlights") {
		// Unsnapped like desktop's --max-lights, for the same bench
		// sweeps; the engine clamps to the list's capacity.
		config.max_lights = n.clamp(1, digblock_client::settings::LIGHT_CAP_MAX);
	}
	if params.get("ssr").is_some() {
		config.ssr = flag(&params, "ssr");
	}
	if let Some(u) = param::<f32>(&params, "uiscale") {
		config.ui_scale = u.clamp(
			digblock_client::settings::UI_SCALE_MIN,
			digblock_client::settings::UI_SCALE_MAX,
		);
	}
	// The menu's Fullscreen row, only where the API actually exists — the
	// shell owns that question, since the shell is what has to act on the
	// answer. iPhone Safari has neither spelling of the API for most of its
	// history; there the menu says how to install the page instead (see
	// index.html), which is that platform's fullscreen. Not while ALREADY
	// installed, where there is no browser chrome left to shed.
	config.fullscreen_button = interactive && digblock_platform::fullscreen_available();
	config.fullscreen_hint =
		interactive && mobile && !config.fullscreen_button && !installed_web_app();
	config.overlay = flag(&params, "overlay");
	// Chunk jobs run in Web Workers when the machine has cores to spare
	// (jobs.rs); `?workers=N` overrides for A/B, and 0 is the pre-pool
	// inline path exactly. The pipeline is told nothing — it upgrades when
	// a pool is attached below — so this only records what to start, and
	// what the debug overlay reports.
	config.workers = param(&params, "workers").map_or_else(jobs::default_count, |n: usize| {
		n.min(jobs::WORKERS_HARD_MAX)
	});
	// Every DIG_* scene and lever the desktop reads from its environment,
	// read here from the URL through the SAME table (`knobs::ALL`), so the
	// browser — the platform whose frames most need attributing, on the
	// GPUs the mobile defaults are chosen for — can pose every capture
	// the desktop can. A typo is fatal, never a silent un-stubbed run:
	// the panic hook prints to the console, this platform's exit(2).
	if let Err(e) = digblock_client::knobs::apply(&mut config, &|k| params.get(k.name)) {
		panic!("{e}");
	}
	// A menu capture is the one screen a capture run would otherwise
	// suppress — the desktop rule, applied after the knobs are known.
	config.menu |= config.debug_screen == Some(digblock_client::DebugScreen::Menu);
	// `?nodatagrams` makes this browser behave like the one that has no
	// datagram channel (iOS Safari), so the reliable-stream fallback can be
	// run on a machine with a console attached. Read here, beside the other
	// capture knobs, and handed to the transport at connect time.
	let no_datagrams = flag(&params, "nodatagrams");
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
	// Redeem a returned sign-in before the engine exists, so the menu opens
	// already signed in and the code leaves the address bar immediately.
	let session = match (&master_url, callback) {
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
	config.account = session.as_ref().map(|s| digblock_client::AccountView {
		id: s.account_id.clone(),
		name: s.display_name.clone(),
	});
	// The pre-account character's lookup key, for servers this browser
	// played on before accounts existed. Only where a join can happen: a
	// capture path has no business reading it.
	if interactive || join {
		config.legacy_identity = legacy_identity();
	}

	let workers = config.workers;
	let mut engine = Engine::new(config);
	if mobile && let Some(insets) = safe_area_insets() {
		engine.set_safe_area(insets);
	}
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
		engine.set_menu_services(services::start(master_url, session, no_datagrams));
	}
	digblock_platform::run(
		engine,
		digblock_platform::RunOptions {
			title: "DigBlock".into(),
			// `?frames=N` ends the run at frame N, which is what makes the
			// bench summary reachable here (see `?bench`): winit's exit on
			// wasm doesn't reliably run destructors, so the platform logs
			// the summary at that boundary rather than from `Drop`.
			exit_after_frames: frames,
			// `?fixeddt=S` — the desktop's `--fixed-dt`, for the same
			// reason: a capture that is compared across builds has to
			// reach frame N in the same world state.
			fixed_dt: param::<f32>(&params, "fixeddt").map(std::time::Duration::from_secs_f32),
			touch: mobile,
			..Default::default()
		},
	)
	.map_err(|e| JsValue::from_str(&e.to_string()))
}

/// The display's safe-area insets (notch, home indicator) in PHYSICAL px — the engine's HUD
/// coordinate space. Script cannot read `env()` directly; resolving it through a probe element's
/// computed padding is the standard route. Read once at boot: the common mobile posture is one
/// orientation per session, and a rotation merely costs the HUD its inset margin until reload.
fn safe_area_insets() -> Option<digblock_client::ui::hud::SafeArea> {
	let window = web_sys::window()?;
	let document = window.document()?;
	let probe = document.create_element("div").ok()?;
	probe
		.set_attribute(
			"style",
			"position:fixed;top:0;left:0;visibility:hidden;\
			 padding-top:env(safe-area-inset-top);\
			 padding-right:env(safe-area-inset-right);\
			 padding-bottom:env(safe-area-inset-bottom);\
			 padding-left:env(safe-area-inset-left);",
		)
		.ok()?;
	document.body()?.append_child(&probe).ok()?;
	let style = window.get_computed_style(&probe).ok().flatten();
	let read = |prop: &str| -> f32 {
		style
			.as_ref()
			.and_then(|s| s.get_property_value(prop).ok())
			.and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
			.unwrap_or(0.0)
	};
	let dpr = window.device_pixel_ratio() as f32;
	let insets = digblock_client::ui::hud::SafeArea {
		top: read("padding-top") * dpr,
		right: read("padding-right") * dpr,
		bottom: read("padding-bottom") * dpr,
		left: read("padding-left") * dpr,
	};
	probe.remove();
	Some(insets)
}

/// Is this page running as an installed home-screen app rather than in a browser tab?
///
/// Two spellings, because iOS predates the standard one: `display-mode` is the media query every
/// other browser answers, `navigator.standalone` is what Safari has always used. Either counts —
/// the caller wants to know whether there is still browser chrome to get rid of.
fn installed_web_app() -> bool {
	let Some(w) = web_sys::window() else {
		return false;
	};
	if w.match_media("(display-mode: standalone)")
		.ok()
		.flatten()
		.is_some_and(|m| m.matches())
	{
		return true;
	}
	js_sys::Reflect::get(&w.navigator(), &JsValue::from_str("standalone"))
		.ok()
		.and_then(|v| v.as_bool())
		.unwrap_or(false)
}

/// A coarse primary pointer AND a touch digitizer — phones and tablets,
/// but not touchscreen laptops, whose primary pointer stays fine. Any API
/// failure answers false: the desktop defaults are the safe fallback.
fn is_mobile_browser() -> bool {
	let Some(w) = web_sys::window() else {
		return false;
	};
	if w.navigator().max_touch_points() <= 0 {
		return false;
	}
	w.match_media("(pointer: coarse)")
		.ok()
		.flatten()
		.is_some_and(|m| m.matches())
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

/// The key this browser's pre-account character is filed under on the servers it used to play on:
/// the truncated SHA-256 of the old identity secret, which localStorage still holds as hex. The
/// desktop twin is `legacy_identity` in apps/desktop and the two must agree byte for byte, which
/// is why the hash itself is `protocol::legacy_identity_hash` and only the STORAGE differs here.
/// Read-only, never created; absent, malformed and all-zeros all answer `None`.
fn legacy_identity() -> Option<[u8; 16]> {
	let hex = web_sys::window()?
		.local_storage()
		.ok()??
		.get_item("digblock.identity")
		.ok()??;
	let key = digblock_net::protocol::legacy_identity_from_hex(&hex)?;
	log::info!("found a pre-account identity — offering it to servers for migration");
	Some(key)
}

/// The Account screen's Copy ID button.
///
/// `navigator.clipboard.writeText` only works inside the task that handled the user's click, which
/// is why the engine drains this from `input` rather than `update`. The returned promise is
/// deliberately dropped: awaiting it would push the result into a later task.
fn copy_to_clipboard(text: &str) {
	let Some(window) = web_sys::window() else {
		return;
	};
	let _ = window.navigator().clipboard().write_text(text);
}

/// The query keys this file reads itself; the scene and lever knobs are
/// `knobs::ALL`. Together they are the page's whole vocabulary.
const PAGE_PARAMS: &[&str] = &[
	"seed",
	"tod",
	"pos",
	"yaw",
	"pitch",
	"frames",
	"fixeddt",
	"bench",
	"fly",
	"warp",
	"walk",
	"capture",
	"play",
	"mobile",
	"renderscale",
	"giscale",
	"draw",
	"maxlights",
	"ssr",
	"uiscale",
	"overlay",
	"workers",
	"nodatagrams",
	"join",
	"server",
	"master",
	"devsignin",
	"mute",
];

/// A key the page does not know is FATAL, like a bad value is. `?vualt` would otherwise photograph
/// open terrain as the vault, and the harness that drives this page passes its flags through by
/// name, so this is where its typos are caught. The sign-in residue (`code`, `state`) is tolerated
/// rather than assumed gone: `take_callback` scrubs the URL only once it has matched a pending
/// request, so a callback landing without one must still boot instead of reading as a typo.
fn reject_unknown_params(params: &web_sys::UrlSearchParams) {
	let known = |k: &str| {
		PAGE_PARAMS.contains(&k)
			|| auth::CALLBACK_PARAMS.contains(&k)
			|| digblock_client::knobs::ALL.iter().any(|n| n.name == k)
	};
	let unknown: Vec<String> = js_sys::try_iter(&params.keys())
		.ok()
		.flatten()
		.into_iter()
		.flatten()
		.filter_map(|k| k.ok().and_then(|k| k.as_string()))
		.filter(|k| !known(k))
		.collect();
	assert!(
		unknown.is_empty(),
		"unknown query param(s): {} — known: {}, and every knob in knobs::ALL",
		unknown.join(", "),
		PAGE_PARAMS.join(", ")
	);
}
