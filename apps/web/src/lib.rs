//! Thin wasm launcher — the browser analogue of apps/desktop/src/main.rs.
//! All real work lives in digblock-render/digblock-platform; this file
//! parses URL query params into an `EngineConfig` and starts the loop.
#![cfg(target_arch = "wasm32")]

use digblock_render::{Engine, EngineConfig};
use wasm_bindgen::prelude::*;

mod services;

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
	// ?overlay, ?name=, ?master= (menu multiplayer), ?join / ?server=
	// (non-interactive join for scripted flows).
	let mut config = EngineConfig::web();
	config.seed = param(&params, "seed");
	config.tod = param(&params, "tod");
	if let Some(d) = param::<i32>(&params, "draw") {
		config.draw_dist = d.clamp(4, 20);
	}
	config.overlay = flag(&params, "overlay");
	config.player_name = params.get("name").filter(|n| !n.is_empty());

	if flag(&params, "join") {
		config.auto_join = Some(params.get("server").unwrap_or_default());
		// The menu refuses to join with an empty name.
		if config.player_name.is_none() {
			config.player_name = Some("web".into());
		}
	}

	let master_url = params.get("master").filter(|m| !m.is_empty());
	let mut engine = Engine::new(config);
	if let Some(master_url) = master_url {
		engine.set_menu_services(services::start(master_url));
	}
	digblock_platform::run(
		engine,
		digblock_platform::RunOptions {
			title: "DigBlock".into(),
			..Default::default()
		},
	)
	.map_err(|e| JsValue::from_str(&e.to_string()))
}
