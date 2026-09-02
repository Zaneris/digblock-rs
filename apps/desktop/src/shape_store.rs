//! The shape editor's file store: one service thread over `std::fs`,
//! answering the engine's `ShapeIoCmd`s. The directory is the repo's
//! committed `assets/shapes/` by default (the texgen precedent: a dev tool
//! run from the checkout writes files that get committed and embedded),
//! overridable with DIG_SHAPE_DIR.

use digblock_client::{ShapeIoCmd, ShapeIoResp, ShapeServices};
use std::path::PathBuf;

fn store_dir() -> PathBuf {
	std::env::var_os("DIG_SHAPE_DIR")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/shapes"))
		})
}

/// A name is a bare file stem, never a path.
fn sane(name: &str) -> Result<(), String> {
	if name.is_empty()
		|| !name
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
	{
		return Err(format!("bad shape name {name:?}"));
	}
	Ok(())
}

fn list(dir: &PathBuf) -> Result<Vec<String>, String> {
	let mut names = Vec::new();
	let entries = match std::fs::read_dir(dir) {
		Ok(e) => e,
		// A missing directory is an empty store, not an error — it is
		// created on first save.
		Err(_) => return Ok(names),
	};
	for entry in entries.flatten() {
		let p = entry.path();
		if p.extension().is_some_and(|e| e == "shape")
			&& let Some(stem) = p.file_stem().and_then(|s| s.to_str())
		{
			names.push(stem.to_string());
		}
	}
	names.sort();
	Ok(names)
}

pub(crate) fn start() -> ShapeServices {
	let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ShapeIoCmd>();
	let (resp_tx, resp_rx) = std::sync::mpsc::channel();
	std::thread::spawn(move || {
		let dir = store_dir();
		for cmd in cmd_rx {
			let resp = match cmd {
				ShapeIoCmd::List => ShapeIoResp::Listing(list(&dir)),
				ShapeIoCmd::Load { name } => {
					let result = sane(&name).and_then(|()| {
						std::fs::read_to_string(dir.join(format!("{name}.shape")))
							.map_err(|e| e.to_string())
					});
					ShapeIoResp::Loaded { name, result }
				}
				ShapeIoCmd::Save { name, text } => {
					let result = sane(&name).and_then(|()| {
						std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
						std::fs::write(dir.join(format!("{name}.shape")), text)
							.map_err(|e| e.to_string())
					});
					ShapeIoResp::Saved { name, result }
				}
				ShapeIoCmd::PickAndLoad => ShapeIoResp::Loaded {
					name: String::new(),
					result: Err(String::from(
						"no picker on desktop: the store dir IS the list",
					)),
				},
			};
			if resp_tx.send(resp).is_err() {
				break;
			}
		}
	});
	ShapeServices { cmd_tx, resp_rx }
}
