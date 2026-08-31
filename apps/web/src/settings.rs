//! Persisted user settings: the web mirror of the desktop's
//! settings.rs, stored as JSON under one localStorage key (the
//! identity-secret pattern). Blocked storage (private mode) degrades
//! to per-session settings.

use digblock_client::settings::Settings;

const KEY: &str = "digblock.settings";

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct SettingsFile {
	draw_dist: Option<i32>,
	music_vol: Option<f32>,
	sfx_vol: Option<f32>,
	ssr: Option<bool>,
	ui_scale: Option<f32>,
	render_scale: Option<f32>,
	gi_scale_div: Option<u32>,
	max_lights: Option<i32>,
	touch_jump_button: Option<bool>,
	show_tutorial: Option<bool>,
}

fn storage() -> Option<web_sys::Storage> {
	web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

/// Overlay saved values onto `base` (the platform defaults). Absent or
/// malformed storage/fields leave the defaults; everything re-clamps.
pub(crate) fn load(base: Settings) -> Settings {
	let file: SettingsFile = storage()
		.and_then(|s| s.get_item(KEY).ok().flatten())
		.and_then(|text| match serde_json::from_str(&text) {
			Ok(f) => Some(f),
			Err(e) => {
				log::warn!("settings in localStorage malformed ({e}) — using defaults");
				None
			}
		})
		.unwrap_or_default();
	Settings {
		draw_dist: file.draw_dist.unwrap_or(base.draw_dist),
		music_vol: file.music_vol.unwrap_or(base.music_vol),
		sfx_vol: file.sfx_vol.unwrap_or(base.sfx_vol),
		ssr: file.ssr.unwrap_or(base.ssr),
		ui_scale: file.ui_scale.unwrap_or(base.ui_scale),
		render_scale: file.render_scale.unwrap_or(base.render_scale),
		gi_scale_div: file.gi_scale_div.unwrap_or(base.gi_scale_div),
		max_lights: file.max_lights.unwrap_or(base.max_lights),
		touch_jump_button: file.touch_jump_button.unwrap_or(base.touch_jump_button),
		show_tutorial: file.show_tutorial.unwrap_or(base.show_tutorial),
	}
	.clamped()
}

/// Fire-and-forget save; a blocked write degrades to per-run settings.
pub(crate) fn save(s: &Settings) {
	let file = SettingsFile {
		draw_dist: Some(s.draw_dist),
		music_vol: Some(s.music_vol),
		sfx_vol: Some(s.sfx_vol),
		ssr: Some(s.ssr),
		ui_scale: Some(s.ui_scale),
		render_scale: Some(s.render_scale),
		gi_scale_div: Some(s.gi_scale_div),
		max_lights: Some(s.max_lights),
		touch_jump_button: Some(s.touch_jump_button),
		show_tutorial: Some(s.show_tutorial),
	};
	let Ok(json) = serde_json::to_string(&file) else {
		return;
	};
	if let Some(storage) = storage()
		&& storage.set_item(KEY, &json).is_err()
	{
		log::warn!("could not persist settings — settings are per-session");
	}
}
