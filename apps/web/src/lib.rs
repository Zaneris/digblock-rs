//! Thin wasm launcher — the browser analogue of apps/desktop/src/main.rs.
//! All real work lives in digblock-render/digblock-platform; this file
//! parses URL query params into an `EngineConfig` and starts the loop.
#![cfg(target_arch = "wasm32")]

use digblock_render::{Engine, EngineConfig};
use wasm_bindgen::prelude::*;

mod audio;
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
	// ?ssr=0, ?shadow=, ?overlay, ?name=, ?master= (menu multiplayer),
	// ?join / ?server= (non-interactive join for scripted flows). The
	// defaults are full desktop quality — these exist to opt weaker
	// hardware down.
	let mut config = EngineConfig::web();
	config.seed = param(&params, "seed");
	config.tod = param(&params, "tod");
	if let Some(d) = param::<i32>(&params, "draw") {
		config.draw_dist = d.clamp(4, 20);
	}
	if params.get("ssr").is_some() {
		config.ssr = flag(&params, "ssr");
	}
	if let Some(s) = param::<u32>(&params, "shadow") {
		config.shadow_size = s.clamp(512, 4096);
	}
	config.overlay = flag(&params, "overlay");
	// ?name= is an explicit override, still held to the 3-10 char rule —
	// an invalid one is treated as absent (the menu field starts empty).
	config.player_name = params
		.get("name")
		.filter(|n| digblock_net::protocol::validate_name(n).is_ok());

	if flag(&params, "join") {
		config.auto_join = Some(params.get("server").unwrap_or_default());
		// The menu refuses to join without a valid name.
		if config.player_name.is_none() {
			config.player_name = Some("web".into());
		}
	}

	config.identity = load_identity();

	let master_url = params.get("master").filter(|m| !m.is_empty());
	let mut engine = Engine::new(config);
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
			..Default::default()
		},
	)
	.map_err(|e| JsValue::from_str(&e.to_string()))
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
