//! Persisted user settings: the launcher-side serde mirror of
//! `digblock_render::settings::Settings` (the engine crate stays
//! serde- and IO-free). JSON in the user's data dir next to the
//! identity file; unknown fields ignored, missing fields default —
//! a future field degrades gracefully in both directions.

use digblock_render::settings::Settings;

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct SettingsFile {
	draw_dist: Option<i32>,
	music_vol: Option<f32>,
	sfx_vol: Option<f32>,
	ssr: Option<bool>,
	ui_scale: Option<f32>,
	/// Last-used multiplayer display name.
	name: Option<String>,
}

fn path() -> std::path::PathBuf {
	dirs::data_dir()
		.map(|d| d.join("digblock").join("settings.json"))
		.unwrap_or_else(|| std::path::PathBuf::from("./digblock-settings.json"))
}

/// Overlay saved values onto `base` (the platform defaults). Absent or
/// malformed file/fields leave the defaults; everything is re-clamped.
pub fn load(base: Settings) -> Settings {
	let file: SettingsFile = match std::fs::read_to_string(path()) {
		Ok(text) => match serde_json::from_str(&text) {
			Ok(f) => f,
			Err(e) => {
				log::warn!(
					"settings file {} malformed ({e}) — using defaults",
					path().display()
				);
				SettingsFile::default()
			}
		},
		Err(_) => SettingsFile::default(),
	};
	Settings {
		draw_dist: file.draw_dist.unwrap_or(base.draw_dist),
		music_vol: file.music_vol.unwrap_or(base.music_vol),
		sfx_vol: file.sfx_vol.unwrap_or(base.sfx_vol),
		ssr: file.ssr.unwrap_or(base.ssr),
		ui_scale: file.ui_scale.unwrap_or(base.ui_scale),
		name: file.name.unwrap_or(base.name),
	}
	.clamped()
}

/// Fire-and-forget save; a failed write degrades to per-run settings
/// (the identity-file policy).
pub fn save(s: &Settings) {
	let file = SettingsFile {
		draw_dist: Some(s.draw_dist),
		music_vol: Some(s.music_vol),
		sfx_vol: Some(s.sfx_vol),
		ssr: Some(s.ssr),
		ui_scale: Some(s.ui_scale),
		name: (!s.name.is_empty()).then(|| s.name.clone()),
	};
	let path = path();
	if let Some(parent) = path.parent() {
		let _ = std::fs::create_dir_all(parent);
	}
	let json = match serde_json::to_string_pretty(&file) {
		Ok(j) => j,
		Err(e) => {
			log::warn!("settings serialize failed: {e}");
			return;
		}
	};
	if let Err(e) = std::fs::write(&path, json) {
		log::warn!(
			"could not persist settings to {} ({e}) — settings are per-run",
			path.display()
		);
	}
}
